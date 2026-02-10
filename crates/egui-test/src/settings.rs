//! Persistent settings and configuration types for the Rumble client.
//!
//! This module contains all settings-related types that can be serialized/deserialized
//! and are independent of the UI framework.

use backend::{AudioSettings, PipelineConfig, SfxKind, VoiceMode};
use clap::Parser;
use directories::ProjectDirs;
use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, fs, path::PathBuf};
use uuid::Uuid;

// =============================================================================
// CLI Arguments
// =============================================================================

/// Rumble - A voice chat client
#[derive(Parser, Debug, Clone, Default)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// Server address to connect to (e.g., 127.0.0.1:5000)
    #[arg(short, long)]
    pub server: Option<String>,

    /// Username for the connection
    #[arg(short, long)]
    pub name: Option<String>,

    /// Password for the server (optional)
    #[arg(short, long)]
    pub password: Option<String>,

    /// Trust the development certificate (dev-certs/server-cert.der)
    #[arg(long, default_value_t = true)]
    pub trust_dev_cert: bool,

    /// Path to a custom server certificate to trust
    #[arg(long)]
    pub cert: Option<String>,
}

// =============================================================================
// Timestamp Formatting
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
    pub fn all() -> &'static [TimestampFormat] {
        &[
            TimestampFormat::Time24h,
            TimestampFormat::Time12h,
            TimestampFormat::DateTime24h,
            TimestampFormat::DateTime12h,
            TimestampFormat::Relative,
        ]
    }

    pub fn label(&self) -> &'static str {
        match self {
            TimestampFormat::Time24h => "24-hour (14:30:05)",
            TimestampFormat::Time12h => "12-hour (2:30:05 PM)",
            TimestampFormat::DateTime24h => "Date + 24-hour",
            TimestampFormat::DateTime12h => "Date + 12-hour",
            TimestampFormat::Relative => "Relative (5m ago)",
        }
    }

    /// Format a SystemTime according to this format.
    pub fn format(&self, time: std::time::SystemTime) -> String {
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

// =============================================================================
// Utility Functions
// =============================================================================

/// Format a byte size as a human-readable string (KB, MB, GB).
pub fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

/// Get an icon for a file based on its MIME type.
pub fn get_file_icon(mime: &str) -> &'static str {
    if mime.starts_with("image/") {
        "🖼"
    } else if mime.starts_with("audio/") {
        "🎵"
    } else if mime.starts_with("video/") {
        "🎬"
    } else if mime.starts_with("text/") {
        "📝"
    } else if mime == "application/pdf" {
        "📕"
    } else if mime.contains("zip") || mime.contains("tar") || mime.contains("rar") || mime.contains("7z") {
        "📦"
    } else if mime.contains("word") || mime.contains("document") {
        "📄"
    } else if mime.contains("excel") || mime.contains("spreadsheet") {
        "📊"
    } else if mime.contains("powerpoint") || mime.contains("presentation") {
        "📽"
    } else {
        "📁"
    }
}

// =============================================================================
// Sound Effects Settings
// =============================================================================

/// Persistent sound effects settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistentSfxSettings {
    pub enabled: bool,
    pub volume: f32,
    pub disabled_sounds: HashSet<SfxKind>,
}

impl Default for PersistentSfxSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            volume: 0.5,
            disabled_sounds: HashSet::new(),
        }
    }
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

    // File transfer settings
    pub file_transfer: FileTransferSettings,

    // Keyboard settings
    pub keyboard: KeyboardSettings,

    // Sound effects settings
    pub sfx: PersistentSfxSettings,
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
            file_transfer: FileTransferSettings::default(),
            keyboard: KeyboardSettings::default(),
            sfx: PersistentSfxSettings::default(),
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
            tracing::warn!("Could not determine config directory, using defaults");
            return Self::default();
        };

        match fs::read_to_string(&path) {
            Ok(contents) => match serde_json::from_str(&contents) {
                Ok(settings) => {
                    tracing::info!("Loaded settings from {}", path.display());
                    settings
                }
                Err(e) => {
                    tracing::warn!("Failed to parse settings file: {}, using defaults", e);
                    Self::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!("No settings file found, using defaults");
                Self::default()
            }
            Err(e) => {
                tracing::warn!("Failed to read settings file: {}, using defaults", e);
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
        let contents =
            serde_json::to_string_pretty(self).map_err(|e| format!("Failed to serialize settings: {}", e))?;

        fs::write(&path, contents).map_err(|e| format!("Failed to write settings file: {}", e))?;

        tracing::info!("Saved settings to {}", path.display());
        Ok(())
    }

    /// Get or generate the Ed25519 signing key.
    /// If no key is stored, generates a new one and stores it in hex format.
    pub fn get_or_generate_signing_key(&mut self) -> SigningKey {
        if let Some(ref hex_key) = self.identity_private_key_hex {
            if let Some(key) = Self::parse_signing_key(hex_key) {
                return key;
            }
            tracing::warn!("Invalid stored signing key, generating new one");
        }

        // Generate a new key
        let key = SigningKey::from_bytes(&rand::random());
        let bytes = key.to_bytes();
        self.identity_private_key_hex = Some(hex::encode(bytes));
        tracing::info!("Generated new Ed25519 identity key");
        key
    }

    /// Parse a signing key from hex string.
    pub fn parse_signing_key(hex_key: &str) -> Option<SigningKey> {
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
        self.identity_private_key_hex
            .as_ref()
            .and_then(|hex| Self::parse_signing_key(hex).map(|key| key.verifying_key().to_bytes()))
    }

    /// Get the public key as hex string for display.
    pub fn public_key_hex(&self) -> Option<String> {
        self.public_key_bytes().map(hex::encode)
    }
}

// =============================================================================
// Audio Settings
// =============================================================================

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

// =============================================================================
// Voice Mode
// =============================================================================

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

// =============================================================================
// File Transfer Settings
// =============================================================================

/// A single auto-download rule with a MIME pattern and max file size.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoDownloadRule {
    /// MIME type pattern (e.g., "image/*", "audio/*", "application/pdf").
    pub mime_pattern: String,
    /// Maximum file size in bytes. 0 = disabled for this pattern.
    pub max_size_bytes: u64,
}

/// File transfer settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FileTransferSettings {
    /// Auto-download files matching the rules below.
    pub auto_download_enabled: bool,
    /// Auto-download rules, each with a MIME pattern and size limit.
    pub auto_download_rules: Vec<AutoDownloadRule>,
    /// Download speed limit in bytes/sec. 0 = unlimited.
    pub download_speed_limit: u64,
    /// Upload speed limit in bytes/sec. 0 = unlimited.
    pub upload_speed_limit: u64,
    /// Continue seeding after download completes.
    pub seed_after_download: bool,
    /// Clean up downloaded files on application exit.
    pub cleanup_on_exit: bool,
    /// Automatically sync chat history when joining a room.
    pub auto_sync_history: bool,
}

impl Default for FileTransferSettings {
    fn default() -> Self {
        Self {
            auto_download_enabled: false,
            auto_download_rules: vec![
                AutoDownloadRule {
                    mime_pattern: "image/*".to_string(),
                    max_size_bytes: 10 * 1024 * 1024, // 10 MB
                },
                AutoDownloadRule {
                    mime_pattern: "audio/*".to_string(),
                    max_size_bytes: 50 * 1024 * 1024, // 50 MB
                },
                AutoDownloadRule {
                    mime_pattern: "text/*".to_string(),
                    max_size_bytes: 1 * 1024 * 1024, // 1 MB
                },
            ],
            download_speed_limit: 0, // Unlimited
            upload_speed_limit: 0,   // Unlimited
            seed_after_download: true,
            cleanup_on_exit: false,
            auto_sync_history: false, // Manual sync by default
        }
    }
}

// =============================================================================
// Certificate Settings
// =============================================================================

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

// =============================================================================
// Keyboard Settings
// =============================================================================

/// Keyboard shortcut configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct KeyboardSettings {
    /// Push-to-talk hotkey binding.
    pub ptt_hotkey: Option<HotkeyBinding>,
    /// Toggle mute hotkey.
    pub toggle_mute_hotkey: Option<HotkeyBinding>,
    /// Toggle deafen hotkey.
    pub toggle_deafen_hotkey: Option<HotkeyBinding>,
    /// Whether global hotkeys are enabled (work when window unfocused).
    pub global_hotkeys_enabled: bool,
}

impl Default for KeyboardSettings {
    fn default() -> Self {
        Self {
            ptt_hotkey: Some(HotkeyBinding {
                modifiers: HotkeyModifiers::default(),
                key: "Space".to_string(),
            }),
            toggle_mute_hotkey: None,
            toggle_deafen_hotkey: None,
            global_hotkeys_enabled: true,
        }
    }
}

/// A hotkey binding with modifiers and key.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HotkeyBinding {
    /// Modifier keys (Ctrl, Shift, Alt, Super/Meta).
    pub modifiers: HotkeyModifiers,
    /// The main key code (e.g., "Space", "F1", "KeyA").
    pub key: String,
}

impl HotkeyBinding {
    /// Format the hotkey for display (e.g., "Ctrl+Shift+Space").
    pub fn display(&self) -> String {
        let mut parts = Vec::new();
        if self.modifiers.ctrl {
            parts.push("Ctrl");
        }
        if self.modifiers.shift {
            parts.push("Shift");
        }
        if self.modifiers.alt {
            parts.push("Alt");
        }
        if self.modifiers.super_key {
            parts.push("Super");
        }
        parts.push(&self.key);
        parts.join("+")
    }
}

/// Modifier keys for hotkey bindings.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct HotkeyModifiers {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub super_key: bool,
}
