use backend::{AudioSettings, BackendHandle, Command, ConnectionState, ConnectConfig, PipelineConfig, ProcessorRegistry, VoiceMode, register_builtin_processors, build_default_tx_pipeline, merge_with_default_tx_pipeline};
use clap::Parser;
use directories::ProjectDirs;
use eframe::egui;
use ed25519_dalek::SigningKey;
use egui::{CollapsingHeader, Modal};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use uuid::Uuid;

mod key_manager;
use key_manager::{FirstRunState, KeyManager, KeySource, KeyInfo, PendingAgentOp, SshAgentClient, connect_and_list_keys, generate_and_add_to_agent};

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

/// Timestamp format options for chat messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum TimestampFormat {
    /// 24-hour format: 14:30:05
    #[default]
    Time24h,
    /// 12-hour format: 2:30:05 PM
    Time12h,
    /// 24-hour with date: 2025-01-15 14:30:05
    DateTime24h,
    /// 12-hour with date: 2025-01-15 2:30:05 PM
    DateTime12h,
    /// Relative time: 5m ago
    Relative,
}

impl TimestampFormat {
    fn all() -> &'static [TimestampFormat] {
        &[
            TimestampFormat::Time24h,
            TimestampFormat::Time12h,
            TimestampFormat::DateTime24h,
            TimestampFormat::DateTime12h,
            TimestampFormat::Relative,
        ]
    }
    
    fn label(&self) -> &'static str {
        match self {
            TimestampFormat::Time24h => "24-hour (14:30:05)",
            TimestampFormat::Time12h => "12-hour (2:30:05 PM)",
            TimestampFormat::DateTime24h => "Date + 24-hour",
            TimestampFormat::DateTime12h => "Date + 12-hour",
            TimestampFormat::Relative => "Relative (5m ago)",
        }
    }
    
    /// Format a SystemTime according to this format.
    fn format(&self, time: std::time::SystemTime) -> String {
        use std::time::{Duration, UNIX_EPOCH};
        
        // Get duration since UNIX epoch
        let duration = time.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
        let secs = duration.as_secs();
        
        // Calculate date/time components (simplified, no timezone handling)
        let days_since_epoch = secs / 86400;
        let time_of_day = secs % 86400;
        let hours = (time_of_day / 3600) as u32;
        let minutes = ((time_of_day % 3600) / 60) as u32;
        let seconds = (time_of_day % 60) as u32;
        
        match self {
            TimestampFormat::Time24h => {
                format!("{:02}:{:02}:{:02}", hours, minutes, seconds)
            }
            TimestampFormat::Time12h => {
                let (h12, ampm) = if hours == 0 {
                    (12, "AM")
                } else if hours < 12 {
                    (hours, "AM")
                } else if hours == 12 {
                    (12, "PM")
                } else {
                    (hours - 12, "PM")
                };
                format!("{}:{:02}:{:02} {}", h12, minutes, seconds, ampm)
            }
            TimestampFormat::DateTime24h | TimestampFormat::DateTime12h => {
                // Calculate year/month/day (simplified algorithm)
                let (year, month, day) = days_to_ymd(days_since_epoch);
                let time_str = if matches!(self, TimestampFormat::DateTime24h) {
                    format!("{:02}:{:02}:{:02}", hours, minutes, seconds)
                } else {
                    let (h12, ampm) = if hours == 0 {
                        (12, "AM")
                    } else if hours < 12 {
                        (hours, "AM")
                    } else if hours == 12 {
                        (12, "PM")
                    } else {
                        (hours - 12, "PM")
                    };
                    format!("{}:{:02}:{:02} {}", h12, minutes, seconds, ampm)
                };
                format!("{:04}-{:02}-{:02} {}", year, month, day, time_str)
            }
            TimestampFormat::Relative => {
                let now = std::time::SystemTime::now();
                let elapsed = now.duration_since(time).unwrap_or(Duration::ZERO);
                let secs = elapsed.as_secs();
                
                if secs < 60 {
                    "just now".to_string()
                } else if secs < 3600 {
                    format!("{}m ago", secs / 60)
                } else if secs < 86400 {
                    format!("{}h ago", secs / 3600)
                } else {
                    format!("{}d ago", secs / 86400)
                }
            }
        }
    }
}

/// Convert days since UNIX epoch to (year, month, day).
fn days_to_ymd(days: u64) -> (i32, u32, u32) {
    // Simplified algorithm for converting days since epoch to Y/M/D
    let days = days as i64;
    let z = days + 719468;
    let era = if z >= 0 { z / 146097 } else { (z - 146096) / 146097 };
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d)
}

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
    
    // Accepted server certificates (stored as hex-encoded fingerprints and DER bytes)
    /// List of accepted certificates: (fingerprint_hex, der_bytes_base64)
    pub accepted_certificates: Vec<AcceptedCertificate>,
    
    // Identity (Ed25519 private key as hex string, 64 hex chars = 32 bytes)
    pub identity_private_key_hex: Option<String>,
    
    // Autoconnect
    pub autoconnect_on_launch: bool,
    
    // Audio settings
    pub audio: PersistentAudioSettings,
    
    // Voice mode
    pub voice_mode: PersistentVoiceMode,
    
    // Selected devices (by ID)
    pub input_device_id: Option<String>,
    pub output_device_id: Option<String>,
    
    // Chat settings
    pub show_chat_timestamps: bool,
    pub chat_timestamp_format: TimestampFormat,
}

/// A certificate that was accepted by the user.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcceptedCertificate {
    /// Server address/name this certificate was accepted for.
    pub server_name: String,
    /// SHA256 fingerprint as hex string.
    pub fingerprint_hex: String,
    /// DER-encoded certificate as base64 string.
    pub certificate_der_base64: String,
}

impl Default for PersistentSettings {
    fn default() -> Self {
        Self {
            server_address: String::new(),
            server_password: String::new(),
            trust_dev_cert: true,
            custom_cert_path: None,
            client_name: format!("user-{}", Uuid::new_v4().simple()),
            accepted_certificates: Vec::new(),
            identity_private_key_hex: None, // Will be generated on first use
            autoconnect_on_launch: false,
            audio: PersistentAudioSettings::default(),
            voice_mode: PersistentVoiceMode::PushToTalk,
            input_device_id: None,
            output_device_id: None,
            show_chat_timestamps: false,
            chat_timestamp_format: TimestampFormat::default(),
        }
    }
}

/// Serializable audio settings (mirrors backend::AudioSettings encoder settings).
/// 
/// Audio processing settings (denoise, VAD, gain) are stored in `tx_pipeline`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistentAudioSettings {
    pub bitrate: i32,
    pub encoder_complexity: i32,
    pub jitter_buffer_delay_packets: u32,
    pub fec_enabled: bool,
    pub packet_loss_percent: i32,
    /// TX pipeline configuration (processors and their settings)
    pub tx_pipeline: Option<PipelineConfig>,
}

impl Default for PersistentAudioSettings {
    fn default() -> Self {
        Self {
            bitrate: 64000,
            encoder_complexity: 10,
            jitter_buffer_delay_packets: 3,
            fec_enabled: true,
            packet_loss_percent: 5,
            tx_pipeline: None,
        }
    }
}

impl From<&AudioSettings> for PersistentAudioSettings {
    fn from(s: &AudioSettings) -> Self {
        Self {
            bitrate: s.bitrate,
            encoder_complexity: s.encoder_complexity,
            jitter_buffer_delay_packets: s.jitter_buffer_delay_packets,
            fec_enabled: s.fec_enabled,
            packet_loss_percent: s.packet_loss_percent,
            // TX pipeline is stored separately, not in AudioSettings
            tx_pipeline: None,
        }
    }
}

impl From<&PersistentAudioSettings> for AudioSettings {
    fn from(s: &PersistentAudioSettings) -> Self {
        Self {
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
    pub fn config_dir() -> Option<PathBuf> {
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
    
    /// Get or generate the Ed25519 signing key.
    /// If no key is stored, generates a new one and stores it in hex format.
    pub fn get_or_generate_signing_key(&mut self) -> SigningKey {
        if let Some(ref hex_key) = self.identity_private_key_hex {
            if let Some(key) = Self::parse_signing_key(hex_key) {
                return key;
            }
            log::warn!("Invalid stored signing key, generating new one");
        }
        
        // Generate a new key
        let key = SigningKey::generate(&mut OsRng);
        let bytes = key.to_bytes();
        self.identity_private_key_hex = Some(hex::encode(bytes));
        log::info!("Generated new Ed25519 identity key");
        key
    }
    
    /// Parse a signing key from hex string.
    fn parse_signing_key(hex_key: &str) -> Option<SigningKey> {
        let bytes = hex::decode(hex_key).ok()?;
        if bytes.len() != 32 {
            return None;
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Some(SigningKey::from_bytes(&arr))
    }
    
    /// Get the public key bytes (32 bytes) if a signing key exists.
    pub fn public_key_bytes(&self) -> Option<[u8; 32]> {
        self.identity_private_key_hex.as_ref().and_then(|hex| {
            Self::parse_signing_key(hex).map(|key| key.verifying_key().to_bytes())
        })
    }
    
    /// Get the public key as hex string for display.
    pub fn public_key_hex(&self) -> Option<String> {
        self.public_key_bytes().map(hex::encode)
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

/// Categories for the settings sidebar
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
enum SettingsCategory {
    #[default]
    Connection,
    Devices,
    Voice,
    Processing,
    Encoder,
    Chat,
    Statistics,
}

impl SettingsCategory {
    fn all() -> &'static [SettingsCategory] {
        &[
            SettingsCategory::Connection,
            SettingsCategory::Devices,
            SettingsCategory::Voice,
            SettingsCategory::Processing,
            SettingsCategory::Encoder,
            SettingsCategory::Chat,
            SettingsCategory::Statistics,
        ]
    }
    
    fn label(&self) -> &'static str {
        match self {
            SettingsCategory::Connection => "🔗 Connection",
            SettingsCategory::Devices => "🔊 Devices",
            SettingsCategory::Voice => "🎤 Voice",
            SettingsCategory::Processing => "⚙ Processing",
            SettingsCategory::Encoder => "📦 Encoder",
            SettingsCategory::Chat => "💬 Chat",
            SettingsCategory::Statistics => "📊 Statistics",
        }
    }
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
}

struct MyApp {
    // UI state
    show_connect: bool,
    show_settings: bool,
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
    
    // Tokio runtime for async operations (SSH agent)
    tokio_runtime: tokio::runtime::Runtime,
    
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
        
        // Get config directory for key manager
        let config_dir = PersistentSettings::config_dir()
            .unwrap_or_else(|| PathBuf::from("."));
        
        // Create key manager and check for migration from old format
        let mut key_manager = KeyManager::new(config_dir);
        
        // Migration: If we have an old-style key in persistent_settings but no new key config,
        // migrate it to the new key_manager format
        if key_manager.needs_setup() {
            if let Some(ref hex_key) = persistent_settings.identity_private_key_hex {
                if let Some(signing_key) = key_manager::parse_signing_key(hex_key) {
                    log::info!("Migrating existing identity key to new format");
                    if let Err(e) = key_manager.import_signing_key(signing_key) {
                        log::error!("Failed to migrate key: {}", e);
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
        
        // Load previously accepted certificates from persistent settings
        {
            use base64::Engine;
            for accepted in &persistent_settings.accepted_certificates {
                match base64::engine::general_purpose::STANDARD.decode(&accepted.certificate_der_base64) {
                    Ok(der_bytes) => {
                        log::info!("Loading accepted certificate for {}", accepted.server_name);
                        config.accepted_certs.push(der_bytes);
                    }
                    Err(e) => {
                        log::warn!("Failed to decode accepted certificate for {}: {}", accepted.server_name, e);
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
        backend.send(Command::UpdateAudioSettings { settings: audio_settings.clone() });
        
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
        backend.send(Command::UpdateTxPipeline { config: tx_pipeline_config });
        
        // Apply device selections if set
        if persistent_settings.input_device_id.is_some() {
            backend.send(Command::SetInputDevice { device_id: persistent_settings.input_device_id.clone() });
        }
        if persistent_settings.output_device_id.is_some() {
            backend.send(Command::SetOutputDevice { device_id: persistent_settings.output_device_id.clone() });
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
            log::warn!("Failed to save settings: {}", e);
        }

        let needs_first_run = !matches!(first_run_state, FirstRunState::NotNeeded);
        
        // Create a Tokio runtime for async operations (SSH agent)
        let tokio_runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("Failed to create Tokio runtime");
        
        let mut app = Self {
            show_connect: false,
            show_settings: false,
            connect_address: server_address,
            connect_password: server_password,
            trust_dev_cert,
            chat_input: String::new(),
            client_name,
            key_manager,
            first_run_state,
            signing_key,
            pending_agent_op: None,
            tokio_runtime,
            persistent_settings,
            autoconnect_on_launch,
            backend,
            processor_registry,
            rename_modal: RenameModalState::default(),
            settings_modal: SettingsModalState::default(),
            push_to_talk_active: false,
            egui_ctx: ctx,
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
        persistent_audio.tx_pipeline = self.settings_modal.pending_tx_pipeline.clone()
            .or_else(|| Some(audio.tx_pipeline.clone()));
        
        self.persistent_settings.audio = persistent_audio;
        self.persistent_settings.voice_mode = self.settings_modal.pending_voice_mode.clone()
            .map(|m| (&m).into())
            .unwrap_or_else(|| (&audio.voice_mode).into());
        self.persistent_settings.input_device_id = audio.selected_input.clone();
        self.persistent_settings.output_device_id = audio.selected_output.clone();
        
        // Chat settings are already in persistent_settings (applied in apply_pending_settings)
        
        if let Err(e) = self.persistent_settings.save() {
            log::error!("Failed to save settings: {}", e);
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

    /// Check if connected based on current state.
    #[allow(dead_code)]
    fn is_connected(&self) -> bool {
        self.backend.state().connection.is_connected()
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
        let mut pending_username = self.settings_modal.pending_username.clone()
            .unwrap_or_else(|| self.client_name.clone());
        if ui.text_edit_singleline(&mut pending_username)
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
        let mut pending_autoconnect = self.settings_modal.pending_autoconnect.unwrap_or(self.autoconnect_on_launch);
        if ui.checkbox(&mut pending_autoconnect, "Autoconnect on launch")
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
            if ui.button("🔑 Generate New Identity...").on_hover_text("Generate a new identity key (will replace the current one)").clicked() {
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
        
        ui.add_space(8.0);
        
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
                let (rect, _response) = ui.allocate_exact_size(
                    egui::vec2(200.0, 16.0),
                    egui::Sense::hover(),
                );
                ui.painter().rect_filled(rect, 2.0, egui::Color32::DARK_GRAY);
                let filled_rect = egui::Rect::from_min_size(
                    rect.min,
                    egui::vec2(rect.width() * normalized, rect.height()),
                );
                ui.painter().rect_filled(filled_rect, 2.0, color);
                
                // Draw VAD threshold line if VAD is enabled
                let pipeline = self.settings_modal.pending_tx_pipeline.as_ref()
                    .unwrap_or(&audio.tx_pipeline);
                let vad_threshold = pipeline.processors.iter()
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
        let pending_voice_mode = self.settings_modal.pending_voice_mode.clone().unwrap_or(audio.voice_mode.clone());
        ui.horizontal(|ui| {
            if ui.selectable_label(
                matches!(pending_voice_mode, VoiceMode::PushToTalk),
                "🎤 Push-to-Talk",
            ).on_hover_text("Hold SPACE to transmit").clicked() {
                self.settings_modal.pending_voice_mode = Some(VoiceMode::PushToTalk);
                self.settings_modal.dirty = true;
            }
            if ui.selectable_label(
                matches!(pending_voice_mode, VoiceMode::Continuous),
                "📡 Continuous",
            ).on_hover_text("Always transmitting when connected. Enable VAD processor for voice-activated behavior.").clicked() {
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
    
    /// Render the Processing settings category.
    fn render_settings_processing(&mut self, ui: &mut egui::Ui, state: &backend::State) {
        ui.heading("Audio Processing");
        ui.add_space(8.0);
        
        ui.label("TX Pipeline - Audio processing chain applied before encoding:");
        ui.add_space(4.0);
        
        // Get processor info from registry for display names
        let processor_info: std::collections::HashMap<&str, (&str, &str)> = self.processor_registry
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
                    if ui.checkbox(&mut proc_config.enabled, display_name)
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
                let (rect, _response) = ui.allocate_exact_size(
                    egui::vec2(200.0, 16.0),
                    egui::Sense::hover(),
                );
                ui.painter().rect_filled(rect, 2.0, egui::Color32::DARK_GRAY);
                let filled_rect = egui::Rect::from_min_size(
                    rect.min,
                    egui::vec2(rect.width() * normalized, rect.height()),
                );
                ui.painter().rect_filled(filled_rect, 2.0, color);
                
                // Draw VAD threshold line if VAD is enabled
                let pipeline = self.settings_modal.pending_tx_pipeline.as_ref()
                    .unwrap_or(&audio.tx_pipeline);
                let vad_threshold = pipeline.processors.iter()
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
            if ui.checkbox(&mut settings.fec_enabled, "Enable Forward Error Correction")
                .on_hover_text("Add redundancy for packet loss recovery")
                .changed() {
                self.settings_modal.dirty = true;
            }
            
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(8.0);
            
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
            
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(8.0);
            
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
            
            ui.add_space(4.0);
            
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
            
            ui.add_space(4.0);
            
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
        let show_timestamps = self.settings_modal.pending_show_timestamps
            .unwrap_or(self.persistent_settings.show_chat_timestamps);
        let mut pending_show = show_timestamps;
        if ui.checkbox(&mut pending_show, "Show timestamps")
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
            let current_format = self.settings_modal.pending_timestamp_format
                .unwrap_or(self.persistent_settings.chat_timestamp_format);
            
            // Ensure pending_timestamp_format is set so we can mutate it
            if self.settings_modal.pending_timestamp_format.is_none() {
                self.settings_modal.pending_timestamp_format = Some(current_format);
            }
            
            egui::ComboBox::from_id_salt("timestamp_format")
                .selected_text(current_format.label())
                .show_ui(ui, |ui| {
                    for format in TimestampFormat::all() {
                        if ui.selectable_value(
                            self.settings_modal.pending_timestamp_format.as_mut().unwrap(),
                            *format,
                            format.label(),
                        ).changed() {
                            self.settings_modal.dirty = true;
                        }
                    }
                });
        });
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
                            ui.colored_label(egui::Color32::YELLOW, "⚠ SSH_AUTH_SOCK not set - ssh-agent not available");
                        }
                        
                        ui.add_enabled_ui(agent_available, |ui| {
                            if ui.button("Connect to SSH Agent →").clicked() {
                                next_state = Some(FirstRunState::ConnectingAgent);
                            }
                        });
                    });
                }
                
                FirstRunState::GenerateLocal { password, password_confirm, error } => {
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
                            
                            if ui.add_enabled(can_generate, egui::Button::new("Generate Key ✓"))
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
                            if ui.add_enabled(can_select, egui::Button::new("Use Selected Key")).clicked() {
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
                let handle = self.tokio_runtime.spawn(connect_and_list_keys());
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
                            match self.tokio_runtime.block_on(handle) {
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
                            match self.tokio_runtime.block_on(handle) {
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
                let handle = self.tokio_runtime.spawn(generate_and_add_to_agent(comment));
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
    let title = prop_schema.get("title")
        .and_then(|t| t.as_str())
        .unwrap_or(key);
    let description = prop_schema.get("description")
        .and_then(|d| d.as_str())
        .unwrap_or("");
    let prop_type = prop_schema.get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("string");
    
    let mut changed = false;
    
    ui.horizontal(|ui| {
        ui.label(format!("{}:", title));
        
        match prop_type {
            "number" => {
                let default = prop_schema.get("default")
                    .and_then(|d| d.as_f64())
                    .unwrap_or(0.0) as f32;
                let min = prop_schema.get("minimum")
                    .and_then(|m| m.as_f64())
                    .unwrap_or(-100.0) as f32;
                let max = prop_schema.get("maximum")
                    .and_then(|m| m.as_f64())
                    .unwrap_or(100.0) as f32;
                
                let mut value = settings.get(key)
                    .and_then(|v| v.as_f64())
                    .map(|v| v as f32)
                    .unwrap_or(default);
                
                if ui.add(egui::Slider::new(&mut value, min..=max))
                    .on_hover_text(description)
                    .changed()
                {
                    settings[key] = serde_json::json!(value);
                    changed = true;
                }
            }
            "integer" => {
                let default = prop_schema.get("default")
                    .and_then(|d| d.as_i64())
                    .unwrap_or(0) as i32;
                let min = prop_schema.get("minimum")
                    .and_then(|m| m.as_i64())
                    .unwrap_or(0) as i32;
                let max = prop_schema.get("maximum")
                    .and_then(|m| m.as_i64())
                    .unwrap_or(1000) as i32;
                
                let mut value = settings.get(key)
                    .and_then(|v| v.as_i64())
                    .map(|v| v as i32)
                    .unwrap_or(default);
                
                if ui.add(egui::Slider::new(&mut value, min..=max))
                    .on_hover_text(description)
                    .changed()
                {
                    settings[key] = serde_json::json!(value);
                    changed = true;
                }
            }
            "boolean" => {
                let default = prop_schema.get("default")
                    .and_then(|d| d.as_bool())
                    .unwrap_or(false);
                let mut value = settings.get(key)
                    .and_then(|v| v.as_bool())
                    .unwrap_or(default);
                
                if ui.checkbox(&mut value, "")
                    .on_hover_text(description)
                    .changed()
                {
                    settings[key] = serde_json::json!(value);
                    changed = true;
                }
            }
            _ => {
                // String or unknown type - show as text field
                let default = prop_schema.get("default")
                    .and_then(|d| d.as_str())
                    .unwrap_or("");
                let mut value = settings.get(key)
                    .and_then(|v| v.as_str())
                    .unwrap_or(default)
                    .to_string();
                
                if ui.text_edit_singleline(&mut value)
                    .on_hover_text(description)
                    .changed()
                {
                    settings[key] = serde_json::json!(value);
                    changed = true;
                }
            }
        }
    });
    
    changed
}

impl eframe::App for MyApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.egui_ctx = ctx.clone();

        // Get current state from backend (clone to avoid borrow issues)
        let state = self.backend.state();

        // Request periodic repaint when settings dialog is open (for level meters)
        // This ensures the UI updates even when no user interaction occurs
        if self.show_settings {
            ctx.request_repaint_after(std::time::Duration::from_millis(50)); // 20 FPS for meters
        }

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
                            egui::ScrollArea::vertical()
                                .auto_shrink([false; 2])
                                .stick_to_bottom(true)
                                .show(ui, |ui| {
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
                                        } else {
                                            // Chat messages from other users
                                            ui.label(format!("{}{}: {}", timestamp_prefix, msg.sender, msg.text));
                                        }
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
                                        
                                        // Local mute indicator (we muted them locally)
                                        let is_locally_muted = state.audio.muted_users.contains(&user_id);
                                        if is_locally_muted && !is_self {
                                            ui.colored_label(egui::Color32::YELLOW, "🔕");
                                        }
                                        
                                        let resp = ui.label(&user.username);
                                        
                                        // Context menu for self user
                                        if is_self {
                                            resp.context_menu(|ui| {
                                                ui.label(format!("User: {} (you)", user.username));
                                                ui.separator();
                                                
                                                // Mute/unmute self
                                                if state.audio.self_muted {
                                                    if ui.button("🎤 Unmute").clicked() {
                                                        self.backend.send(Command::SetMuted { muted: false });
                                                        ui.close();
                                                    }
                                                } else {
                                                    if ui.button("🔇 Mute").clicked() {
                                                        self.backend.send(Command::SetMuted { muted: true });
                                                        ui.close();
                                                    }
                                                }
                                                
                                                // Deafen/undeafen self
                                                if state.audio.self_deafened {
                                                    if ui.button("🔊 Undeafen").clicked() {
                                                        self.backend.send(Command::SetDeafened { deafened: false });
                                                        ui.close();
                                                    }
                                                } else {
                                                    if ui.button("🔇 Deafen").clicked() {
                                                        self.backend.send(Command::SetDeafened { deafened: true });
                                                        ui.close();
                                                    }
                                                }
                                                
                                                ui.separator();
                                                
                                                // Registration
                                                if ui.button("📝 Register").clicked() {
                                                    self.backend.send(Command::RegisterUser { user_id });
                                                    ui.close();
                                                }
                                                if ui.button("❌ Unregister").clicked() {
                                                    self.backend.send(Command::UnregisterUser { user_id });
                                                    ui.close();
                                                }
                                            });
                                        }
                                        
                                        // Context menu for other users (not self)
                                        if !is_self {
                                            resp.context_menu(|ui| {
                                                ui.label(format!("User: {}", user.username));
                                                ui.separator();
                                                
                                                // Local mute toggle
                                                if is_locally_muted {
                                                    if ui.button("🔔 Unmute Locally").clicked() {
                                                        self.backend.send(Command::UnmuteUser { user_id });
                                                        ui.close();
                                                    }
                                                } else {
                                                    if ui.button("🔕 Mute Locally").clicked() {
                                                        self.backend.send(Command::MuteUser { user_id });
                                                        ui.close();
                                                    }
                                                }
                                                
                                                ui.separator();
                                                
                                                // Volume adjustment
                                                ui.label("Volume:");
                                                let current_volume = state.audio.per_user_rx
                                                    .get(&user_id)
                                                    .map(|c| c.volume_db)
                                                    .unwrap_or(0.0);
                                                let mut volume = current_volume;
                                                if ui.add(egui::Slider::new(&mut volume, -20.0..=12.0)
                                                    .suffix(" dB")
                                                    .text(""))
                                                    .changed()
                                                {
                                                    self.backend.send(Command::SetUserVolume { 
                                                        user_id, 
                                                        volume_db: volume 
                                                    });
                                                }
                                                
                                                // Reset volume button
                                                if current_volume != 0.0 {
                                                    if ui.button("Reset Volume").clicked() {
                                                        self.backend.send(Command::SetUserVolume { 
                                                            user_id, 
                                                            volume_db: 0.0 
                                                        });
                                                        ui.close();
                                                    }
                                                }
                                                
                                                ui.separator();
                                                
                                                // Registration
                                                if ui.button("📝 Register").clicked() {
                                                    self.backend.send(Command::RegisterUser { user_id });
                                                    ui.close();
                                                }
                                                if ui.button("❌ Unregister").clicked() {
                                                    self.backend.send(Command::UnregisterUser { user_id });
                                                    ui.close();
                                                }
                                            });
                                        }
                                    });
                                }
                            }
                        });
                    ui.separator();
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
                ui.label(egui::RichText::new(
                    "The server presented a certificate that is not trusted."
                ).color(egui::Color32::YELLOW));
                
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
                ui.add(egui::TextEdit::multiline(&mut fingerprint.as_str())
                    .font(egui::TextStyle::Monospace)
                    .desired_width(f32::INFINITY)
                    .desired_rows(2)
                    .interactive(false));
                
                ui.add_space(8.0);
                ui.label(egui::RichText::new(
                    "If you expected to connect to a server with a self-signed certificate, \
                     verify the fingerprint matches what the server administrator provided."
                ).weak());
                
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
                                certificate_der_base64: base64::engine::general_purpose::STANDARD.encode(&certificate_der),
                            };
                            
                            // Add to persistent settings if not already present
                            if !self.persistent_settings.accepted_certificates.iter()
                                .any(|c| c.fingerprint_hex == fingerprint)
                            {
                                self.persistent_settings.accepted_certificates.push(accepted_cert);
                                if let Err(e) = self.persistent_settings.save() {
                                    log::error!("Failed to save accepted certificate: {}", e);
                                }
                            }
                            
                            self.backend.send(Command::AcceptCertificate);
                            self.backend.send(Command::LocalMessage {
                                text: format!("Accepted certificate for {} (saved for future connections)", server_name),
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
                };
            }
            
            let modal = Modal::new(egui::Id::new("settings_modal"))
                .show(ctx, |ui| {
                ui.set_min_size(egui::vec2(600.0, 400.0));
                ui.set_max_size(egui::vec2(800.0, 600.0));
                
                // Header
                ui.horizontal(|ui| {
                    ui.heading("Settings");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self.settings_modal.dirty {
                            ui.colored_label(egui::Color32::YELLOW, "⚠ Unsaved changes");
                        }
                    });
                });
                ui.separator();
                
                // Main content area with sidebar and content panel
                egui::TopBottomPanel::bottom("settings_footer")
                    .frame(egui::Frame::NONE)
                    .show_inside(ui, |ui| {
                        ui.add_space(4.0);
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
                                    self.apply_pending_settings();
                                    self.settings_modal.dirty = false;
                                    self.save_settings();
                                }
                                
                                if ui.button("Cancel").clicked() {
                                    self.settings_modal = SettingsModalState::default();
                                    self.show_settings = false;
                                }
                                
                                if ui.button("Ok").clicked() {
                                    // Apply changes before closing if dirty
                                    if self.settings_modal.dirty {
                                        self.apply_pending_settings();
                                        self.save_settings();
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
                        egui::ScrollArea::vertical()
                            .id_salt("settings_sidebar")
                            .show(ui, |ui| {
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
                        egui::ScrollArea::vertical()
                            .id_salt("settings_content")
                            .show(ui, |ui| {
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
                                            SettingsCategory::Processing => {
                                                self.render_settings_processing(ui, &state);
                                            }
                                            SettingsCategory::Encoder => {
                                                self.render_settings_encoder(ui);
                                            }
                                            SettingsCategory::Chat => {
                                                self.render_settings_chat(ui);
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
    }
}
