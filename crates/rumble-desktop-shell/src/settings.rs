//! Settings schema + JSON persistence for the desktop client.
//!
//! The schema grows as features land — fields cover UI choices and
//! connection state such as `paradigm`, `dark`, `recent_servers`, and
//! `accepted_certificates`.
//!
//! Forward / backward compatibility:
//! - `#[serde(default)]` on the struct → missing fields use defaults,
//!   so older config files load cleanly when we add new fields.
//! - The `_extra` map captures unknown fields and round-trips them on
//!   save, so when a *newer* client writes fields this version doesn't
//!   know about, an older client won't silently delete them.
//!
//! The store path defaults to `<ProjectDirs>/desktop-shell.json`.

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use directories::ProjectDirs;
use rumble_client::{AudioSettings, PipelineConfig, SfxKind, VoiceMode};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::hotkeys::KeyboardSettings;

/// How chat timestamps are rendered in the UI.
///
/// Variant set + ordering match `rumble-egui`'s `TimestampFormat` so
/// settings files port cleanly between clients. Default is `Time24h`,
/// which matches Mumble's longstanding behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum TimestampFormat {
    #[default]
    Time24h,
    Time12h,
    DateTime24h,
    DateTime12h,
    Relative,
}

impl TimestampFormat {
    pub const ALL: &'static [TimestampFormat] = &[
        TimestampFormat::Time24h,
        TimestampFormat::Time12h,
        TimestampFormat::DateTime24h,
        TimestampFormat::DateTime12h,
        TimestampFormat::Relative,
    ];

    pub fn label(self) -> &'static str {
        match self {
            TimestampFormat::Time24h => "24-hour (14:30:05)",
            TimestampFormat::Time12h => "12-hour (2:30:05 PM)",
            TimestampFormat::DateTime24h => "Date + 24-hour",
            TimestampFormat::DateTime12h => "Date + 12-hour",
            TimestampFormat::Relative => "Relative (5m ago)",
        }
    }

    /// Format `time` as a human-readable string per the selected
    /// `TimestampFormat`. Times are rendered in UTC since the rumble
    /// shell does not depend on a timezone crate; the variant labels
    /// already advertise this (e.g. *"24-hour (14:30:05)"*).
    pub fn format(self, time: std::time::SystemTime) -> String {
        use std::time::{Duration, UNIX_EPOCH};

        let duration = time.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
        let secs = duration.as_secs();
        let days_since_epoch = secs / 86_400;
        let time_of_day = secs % 86_400;
        let hours = (time_of_day / 3_600) as u32;
        let minutes = ((time_of_day % 3_600) / 60) as u32;
        let seconds = (time_of_day % 60) as u32;

        let to_12h = |h: u32| -> (u32, &'static str) {
            if h == 0 {
                (12, "AM")
            } else if h < 12 {
                (h, "AM")
            } else if h == 12 {
                (12, "PM")
            } else {
                (h - 12, "PM")
            }
        };

        match self {
            TimestampFormat::Time24h => format!("{hours:02}:{minutes:02}:{seconds:02}"),
            TimestampFormat::Time12h => {
                let (h12, ampm) = to_12h(hours);
                format!("{h12}:{minutes:02}:{seconds:02} {ampm}")
            }
            TimestampFormat::DateTime24h | TimestampFormat::DateTime12h => {
                let (year, month, day) = days_to_ymd(days_since_epoch);
                let time_str = if matches!(self, TimestampFormat::DateTime24h) {
                    format!("{hours:02}:{minutes:02}:{seconds:02}")
                } else {
                    let (h12, ampm) = to_12h(hours);
                    format!("{h12}:{minutes:02}:{seconds:02} {ampm}")
                };
                format!("{year:04}-{month:02}-{day:02} {time_str}")
            }
            TimestampFormat::Relative => {
                let now = std::time::SystemTime::now();
                let elapsed = now.duration_since(time).unwrap_or(Duration::ZERO).as_secs();
                if elapsed < 60 {
                    "just now".to_string()
                } else if elapsed < 3_600 {
                    format!("{}m ago", elapsed / 60)
                } else if elapsed < 86_400 {
                    format!("{}h ago", elapsed / 3_600)
                } else {
                    format!("{}d ago", elapsed / 86_400)
                }
            }
        }
    }
}

/// Convert days since the UNIX epoch (1970-01-01) to `(year, month, day)`
/// in the proleptic Gregorian calendar (UTC). Implementation is the
/// standard "days_from_civil" inverse from Howard Hinnant's date
/// algorithms — matches the egui client byte-for-byte.
fn days_to_ymd(days: u64) -> (i32, u32, u32) {
    let days = days as i64;
    let z = days + 719_468;
    let era = if z >= 0 { z / 146_097 } else { (z - 146_096) / 146_097 };
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d)
}

/// Sound-effect playback preferences. Volume applies on top of the
/// per-call `Command::PlaySfx { volume }` parameter, so `1.0` means
/// "play at the volume the caller asked for"; `0.0` mutes everything
/// without disabling the toggle.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SfxSettings {
    pub enabled: bool,
    pub volume: f32,
    /// Per-event opt-out: kinds in this set are suppressed even when
    /// `enabled` is true. Lets users keep e.g. mute clicks while
    /// silencing noisy join/leave chimes.
    pub disabled_sounds: HashSet<SfxKind>,
}

impl Default for SfxSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            volume: 0.6,
            disabled_sounds: HashSet::new(),
        }
    }
}

impl SfxSettings {
    pub fn is_kind_enabled(&self, kind: SfxKind) -> bool {
        self.enabled && !self.disabled_sounds.contains(&kind)
    }
}

/// Persistent voice activation mode. Mirrors `rumble_client::VoiceMode`
/// but lives here so we don't leak a serde-Deserialize requirement onto
/// the wire type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PersistentVoiceMode {
    #[default]
    PushToTalk,
    Continuous,
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

/// Persistent encoder + pipeline configuration. Field set mirrors
/// `rumble_client::AudioSettings` so settings written by `rumble-egui`
/// load cleanly here and vice versa. The TX pipeline is stored as a
/// nested option — `None` means "use the default pipeline at startup".
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistentAudioSettings {
    pub bitrate: i32,
    pub encoder_complexity: i32,
    pub jitter_buffer_delay_packets: u32,
    pub fec_enabled: bool,
    pub packet_loss_percent: i32,
    /// TX pipeline configuration (processors and their settings).
    pub tx_pipeline: Option<PipelineConfig>,
}

impl Default for PersistentAudioSettings {
    fn default() -> Self {
        let defaults = AudioSettings::default();
        Self {
            bitrate: defaults.bitrate,
            encoder_complexity: defaults.encoder_complexity,
            jitter_buffer_delay_packets: defaults.jitter_buffer_delay_packets,
            fec_enabled: defaults.fec_enabled,
            packet_loss_percent: defaults.packet_loss_percent,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ChatSettings {
    /// Whether to render `[hh:mm]` style timestamps next to messages.
    pub show_timestamps: bool,
    /// Format used when timestamps are visible.
    pub timestamp_format: TimestampFormat,
    /// On joining a room, ask peers for their backlog so the user
    /// sees what was said before they arrived.
    pub auto_sync_history: bool,
    /// Auto-play animated images (GIFs) inline in the chat log.
    /// Off shows the first frame only, with a play overlay.
    #[serde(default = "default_gif_autoplay")]
    pub gif_autoplay: bool,
}

fn default_gif_autoplay() -> bool {
    true
}

impl Default for ChatSettings {
    fn default() -> Self {
        Self {
            show_timestamps: false,
            timestamp_format: TimestampFormat::default(),
            auto_sync_history: false,
            gif_autoplay: default_gif_autoplay(),
        }
    }
}

/// One auto-download rule: files matching `mime_pattern` are pulled
/// automatically when offered, up to `max_size_bytes`. `0` disables
/// the rule without removing it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoDownloadRule {
    pub mime_pattern: String,
    pub max_size_bytes: u64,
}

impl AutoDownloadRule {
    /// True if this rule should auto-accept an offer with the given
    /// MIME type and size. `max_size_bytes == 0` is treated as
    /// "rule disabled" so users can keep a pattern around without it
    /// firing.
    pub fn matches(&self, mime: &str, size: u64) -> bool {
        if self.max_size_bytes == 0 || size > self.max_size_bytes {
            return false;
        }
        mime_pattern_matches(&self.mime_pattern, mime)
    }
}

/// Match a glob-ish MIME pattern. Supports `*` / `*/*` (everything),
/// `<top>/*` (top-level type prefix), and exact equality. Comparisons
/// are case-insensitive — MIME type names are.
fn mime_pattern_matches(pattern: &str, mime: &str) -> bool {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return false;
    }
    if pattern == "*" || pattern == "*/*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix("/*") {
        return mime
            .split_once('/')
            .map(|(top, _)| top.eq_ignore_ascii_case(prefix))
            .unwrap_or(false);
    }
    pattern.eq_ignore_ascii_case(mime)
}

/// File-transfer preferences shared across desktop GUI clients.
///
/// `auto_download_enabled` + `auto_download_rules` are consumed by
/// the incoming-offer pump. The bandwidth caps are persisted UI state
/// today and will feed the file-transfer plugin once that surface lands.
/// `download_dir` overrides the platform default (system temp dir +
/// `rumble_downloads`) for fetched files.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FileTransferSettings {
    pub auto_download_enabled: bool,
    pub auto_download_rules: Vec<AutoDownloadRule>,
    /// Download speed limit in bytes/sec; 0 = unlimited.
    pub download_speed_limit: u64,
    /// Upload speed limit in bytes/sec; 0 = unlimited.
    pub upload_speed_limit: u64,
    /// Directory where fetched files are saved. `None` = use the
    /// platform default. Applied at next connect (the relay plugin
    /// captures the value when it's constructed).
    pub download_dir: Option<PathBuf>,
}

impl FileTransferSettings {
    /// True if any active rule matches and auto-download is enabled.
    /// Convenience for the receive path: one call per incoming offer.
    pub fn should_auto_download(&self, mime: &str, size: u64) -> bool {
        self.auto_download_enabled && self.auto_download_rules.iter().any(|r| r.matches(mime, size))
    }
}

impl Default for FileTransferSettings {
    fn default() -> Self {
        Self {
            auto_download_enabled: false,
            auto_download_rules: vec![
                AutoDownloadRule {
                    mime_pattern: "image/*".to_string(),
                    max_size_bytes: 10 * 1024 * 1024,
                },
                AutoDownloadRule {
                    mime_pattern: "audio/*".to_string(),
                    max_size_bytes: 50 * 1024 * 1024,
                },
                AutoDownloadRule {
                    mime_pattern: "text/*".to_string(),
                    max_size_bytes: 1024 * 1024,
                },
            ],
            download_speed_limit: 0,
            upload_speed_limit: 0,
            download_dir: None,
        }
    }
}

/// Default location for the shared shell settings file.
///
/// Returns `None` only on platforms where `directories` cannot resolve
/// a config dir; callers should treat that as "settings disabled" and
/// run with defaults.
pub fn default_settings_path() -> Option<PathBuf> {
    ProjectDirs::from("network", "gecko", "Rumble").map(|dirs| dirs.config_dir().join("desktop-shell.json"))
}

/// Server the user has connected to before. Populated as the connect
/// flow learns about new servers; consumed by the upcoming recent-
/// servers UI (bringup doc §2).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RecentServer {
    /// Address used to dial (e.g. `127.0.0.1:5000`).
    pub addr: String,
    /// User-chosen display name for the server, or empty if none.
    #[serde(default)]
    pub label: String,
    /// Username last used against this server.
    #[serde(default)]
    pub username: String,
    /// Last connect timestamp (unix seconds), used to sort the list.
    #[serde(default)]
    pub last_used_unix: u64,
}

/// A server certificate the user has approved.
///
/// Field names match `rumble-egui`'s `AcceptedCertificate` so the
/// schemas line up when rumble-egui migrates to this store.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AcceptedCertificate {
    /// Server address/name the certificate was accepted for.
    pub server_name: String,
    /// SHA256 fingerprint as hex string.
    pub fingerprint_hex: String,
    /// DER-encoded certificate as base64 string.
    pub certificate_der_base64: String,
}

impl AcceptedCertificate {
    /// Build an entry from raw DER bytes. Base64-encodes for storage.
    pub fn from_der(server_name: impl Into<String>, fingerprint_hex: impl Into<String>, der: &[u8]) -> Self {
        use base64::{Engine, engine::general_purpose::STANDARD};
        Self {
            server_name: server_name.into(),
            fingerprint_hex: fingerprint_hex.into(),
            certificate_der_base64: STANDARD.encode(der),
        }
    }

    /// Decode the stored cert back to DER bytes. Returns `None` if the
    /// stored base64 is malformed.
    pub fn der_bytes(&self) -> Option<Vec<u8>> {
        use base64::{Engine, engine::general_purpose::STANDARD};
        STANDARD.decode(&self.certificate_der_base64).ok()
    }
}

/// Persistent settings shared by all desktop GUI clients.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Active paradigm (e.g. `"Modern"`). Stored as a string so this
    /// crate doesn't need to know the enum's variants.
    pub paradigm: Option<String>,
    /// Dark mode toggle.
    pub dark: bool,
    /// Servers the user has connected to before, newest last.
    pub recent_servers: Vec<RecentServer>,
    /// Approved server certificates, keyed implicitly by `server_name`.
    pub accepted_certificates: Vec<AcceptedCertificate>,
    /// Server address to auto-connect on launch. Must match a
    /// `RecentServer.addr` to take effect; clearing this disables
    /// auto-connect.
    pub auto_connect_addr: Option<String>,
    /// Hotkey bindings (PTT, mute, deafen) and the global-hotkey
    /// enable flag. Defaults to PTT=Space.
    pub keyboard: KeyboardSettings,
    /// Chat display preferences.
    pub chat: ChatSettings,
    /// Sound effect playback preferences.
    pub sfx: SfxSettings,
    /// Opus encoder + TX pipeline preferences. Applied at startup so
    /// the audio system comes up exactly as the user left it.
    pub audio: PersistentAudioSettings,
    /// File-transfer preferences (auto-download rules, bandwidth caps,
    /// seed-after-download). Persisted alongside chat settings.
    pub file_transfer: FileTransferSettings,
    /// Voice activation mode (PTT vs continuous).
    pub voice_mode: PersistentVoiceMode,
    /// Last selected input device ID. `None` = system default.
    pub input_device_id: Option<String>,
    /// Last selected output device ID. `None` = system default.
    pub output_device_id: Option<String>,

    /// Catch-all for fields written by a newer client. Round-tripped
    /// on save so we don't silently delete unknown settings.
    #[serde(flatten)]
    _extra: HashMap<String, Value>,
}

/// Write `contents` to `path` atomically via a sibling temp file.
///
/// Writes to `<path>.tmp` in the same directory, calls `sync_all`, then
/// renames into place. A crash between write and rename leaves the original
/// intact. The directory is assumed to already exist (callers ensure this).
fn atomic_write(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp_path = std::path::PathBuf::from(tmp_name);

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp_path)?;
    file.write_all(contents)?;
    file.sync_all()?;
    fs::rename(&tmp_path, path)
}

/// Attempt to rename `path` to `<path>.corrupt` before proceeding with
/// defaults. Returns `true` if the rename succeeded — the caller may keep
/// the original path active because the file has been moved aside and the
/// next save will create a fresh one there. Returns `false` if rename
/// failed; the original bytes are still at `path` and the caller must
/// disable persistence to prevent overwriting them.
fn backup_corrupt_settings_file(path: &Path) -> bool {
    let mut backup_name = path.as_os_str().to_owned();
    backup_name.push(".corrupt");
    let backup = PathBuf::from(backup_name);
    match fs::rename(path, &backup) {
        Ok(()) => {
            tracing::warn!("settings: corrupt file preserved as {}", backup.display());
            true
        }
        Err(e) => {
            tracing::warn!(
                "settings: could not rename to backup {}: {e} — disabling persistence for this session to protect \
                 original",
                backup.display()
            );
            false
        }
    }
}

/// In-memory wrapper that loads `Settings` from disk and saves changes
/// after each mutation.
///
/// Save is synchronous and runs on every `modify()`. The file is small
/// (< 4 KB once populated), so debouncing buys us nothing yet — we can
/// add it later if a settings-heavy UI starts pegging the disk.
pub struct SettingsStore {
    settings: Settings,
    path: Option<PathBuf>,
}

impl SettingsStore {
    /// Load from the default path (`<config>/desktop-shell.json`).
    /// Always returns a usable store: parse errors and missing files
    /// fall through to defaults, with a warning logged.
    pub fn load_default() -> Self {
        Self::load_from_path(default_settings_path())
    }

    /// Load from an explicit path. Pass `None` to run in-memory only
    /// (useful for tests / headless renders).
    pub fn load_from_path(path: Option<PathBuf>) -> Self {
        let Some(path) = path else {
            tracing::debug!("settings: no path configured, running in-memory");
            return Self {
                settings: Self::initialised_defaults(),
                path: None,
            };
        };

        // active_path is normally Some(path). It is set to None when a corrupt
        // file could not be renamed to the backup — in that case the original
        // bytes are still at path and saves must be suppressed to protect them.
        let (mut settings, active_path) = match fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Settings>(&text) {
                Ok(s) => {
                    tracing::info!("settings: loaded {}", path.display());
                    (s, Some(path))
                }
                Err(err) => {
                    // File is present but unparseable. Rename it aside before
                    // proceeding with defaults so the next save cannot destroy it.
                    tracing::warn!("settings: failed to parse {}: {err} — using defaults", path.display());
                    let active = if backup_corrupt_settings_file(&path) {
                        Some(path)
                    } else {
                        None
                    };
                    (Self::initialised_defaults(), active)
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!("settings: {} not found, using defaults", path.display());
                (Self::initialised_defaults(), Some(path))
            }
            Err(err) => {
                // File exists but cannot be read. Rename it aside for the same
                // reason as the parse-failure case above.
                tracing::warn!("settings: could not read {}: {err} — using defaults", path.display());
                let active = if backup_corrupt_settings_file(&path) {
                    Some(path)
                } else {
                    None
                };
                (Self::initialised_defaults(), active)
            }
        };

        // Drain pre-shortcuts hotkey fields into the new vec. No default
        // shortcut entries are seeded — that auto-seed used to write a
        // Space → Push-to-Talk row on first launch, but binding a global
        // hotkey without the user asking can collide with their existing
        // keymap (and on Wayland it would also fire the portal prompt at
        // app startup). Users add their own shortcuts from the Shortcuts
        // settings tab.
        settings.keyboard.normalize_legacy();

        Self {
            settings,
            path: active_path,
        }
    }

    /// Construct the in-memory default `Settings`. Matches the on-disk
    /// path: legacy fields are drained on the fly elsewhere, no shortcut
    /// rows are seeded here.
    fn initialised_defaults() -> Settings {
        Settings::default()
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Run `f` against the inner `Settings`, then persist if anything
    /// changed. Returns `f`'s value so callers can chain.
    ///
    /// Save errors are logged but not surfaced — settings persistence
    /// is best-effort, and we'd rather drop a save than break the UI.
    pub fn modify<R>(&mut self, f: impl FnOnce(&mut Settings) -> R) -> R {
        let result = f(&mut self.settings);
        self.save();
        result
    }

    /// Force a save without modifying anything (e.g. during shutdown).
    pub fn save(&self) {
        let Some(path) = &self.path else { return };
        if let Some(parent) = path.parent()
            && let Err(err) = fs::create_dir_all(parent)
        {
            tracing::warn!("settings: could not create {}: {err}", parent.display());
            return;
        }
        let json = match serde_json::to_string_pretty(&self.settings) {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!("settings: serialize failed: {err}");
                return;
            }
        };
        if let Err(err) = atomic_write(path, json.as_bytes()) {
            tracing::warn!("settings: write to {} failed: {err}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_unknown_fields() {
        // A newer client wrote `future_field`. Loading it into our
        // schema should preserve it on the next save.
        let json = r#"{ "dark": true, "future_field": {"nested": 1} }"#;
        let parsed: Settings = serde_json::from_str(json).unwrap();
        assert!(parsed.dark);

        let reserialized = serde_json::to_string(&parsed).unwrap();
        assert!(reserialized.contains("future_field"));
        assert!(reserialized.contains("nested"));
    }

    #[test]
    fn defaults_when_file_missing() {
        let store = SettingsStore::load_from_path(Some(PathBuf::from(
            "/tmp/rumble-desktop-shell-nonexistent-test-file.json",
        )));
        assert!(store.settings().paradigm.is_none());
        assert!(!store.settings().dark);
        assert!(store.settings().recent_servers.is_empty());
    }

    #[test]
    fn modify_saves() {
        let dir = tempdir_for_test();
        let path = dir.join("settings.json");
        {
            let mut store = SettingsStore::load_from_path(Some(path.clone()));
            store.modify(|s| {
                s.dark = true;
                s.paradigm = Some("Luna".into());
            });
        }
        let reread = SettingsStore::load_from_path(Some(path));
        assert!(reread.settings().dark);
        assert_eq!(reread.settings().paradigm.as_deref(), Some("Luna"));
    }

    #[test]
    fn audio_settings_round_trip() {
        // Audio settings written by one session must come back intact
        // — the entire restore-on-startup path depends on this.
        let dir = tempdir_for_test();
        let path = dir.join("settings.json");
        {
            let mut store = SettingsStore::load_from_path(Some(path.clone()));
            store.modify(|s| {
                s.audio.bitrate = 32000;
                s.audio.encoder_complexity = 5;
                s.audio.fec_enabled = false;
                s.audio.packet_loss_percent = 12;
                s.voice_mode = PersistentVoiceMode::Continuous;
                s.input_device_id = Some("usb-mic".into());
                s.output_device_id = Some("hdmi-out".into());
                s.sfx.disabled_sounds.insert(SfxKind::UserJoin);
                s.sfx.disabled_sounds.insert(SfxKind::UserLeave);
            });
        }
        let reread = SettingsStore::load_from_path(Some(path));
        let s = reread.settings();
        assert_eq!(s.audio.bitrate, 32000);
        assert_eq!(s.audio.encoder_complexity, 5);
        assert!(!s.audio.fec_enabled);
        assert_eq!(s.audio.packet_loss_percent, 12);
        assert_eq!(s.voice_mode, PersistentVoiceMode::Continuous);
        assert_eq!(s.input_device_id.as_deref(), Some("usb-mic"));
        assert_eq!(s.output_device_id.as_deref(), Some("hdmi-out"));
        assert!(s.sfx.disabled_sounds.contains(&SfxKind::UserJoin));
        assert!(s.sfx.disabled_sounds.contains(&SfxKind::UserLeave));
        assert!(!s.sfx.disabled_sounds.contains(&SfxKind::Connect));
        assert!(s.sfx.is_kind_enabled(SfxKind::Connect));
        assert!(!s.sfx.is_kind_enabled(SfxKind::UserJoin));
    }

    #[test]
    fn auto_download_rule_matches_glob_and_size() {
        let rule = AutoDownloadRule {
            mime_pattern: "image/*".into(),
            max_size_bytes: 1024,
        };
        assert!(rule.matches("image/png", 512));
        assert!(rule.matches("IMAGE/JPEG", 1024));
        assert!(!rule.matches("image/png", 2048), "oversize should not match");
        assert!(!rule.matches("audio/ogg", 256), "wrong type should not match");

        let exact = AutoDownloadRule {
            mime_pattern: "application/pdf".into(),
            max_size_bytes: 10,
        };
        assert!(exact.matches("application/pdf", 5));
        assert!(!exact.matches("application/json", 5));

        let zero = AutoDownloadRule {
            mime_pattern: "*/*".into(),
            max_size_bytes: 0,
        };
        assert!(!zero.matches("image/png", 1), "0 max disables the rule");

        let star = AutoDownloadRule {
            mime_pattern: "*".into(),
            max_size_bytes: 100,
        };
        assert!(star.matches("anything/anything", 1));
    }

    #[test]
    fn file_transfer_should_auto_download_respects_enable_flag() {
        let mut s = FileTransferSettings::default();
        // Defaults include image/* up to 10 MB.
        assert!(!s.should_auto_download("image/png", 1024), "disabled by default");
        s.auto_download_enabled = true;
        assert!(s.should_auto_download("image/png", 1024));
        assert!(!s.should_auto_download("image/png", 50 * 1024 * 1024), "exceeds cap");
        assert!(!s.should_auto_download("video/mp4", 1024), "no rule matches");
    }

    /// save() must produce a valid, parseable file and leave no .tmp behind.
    #[test]
    fn atomic_save_produces_parseable_file_no_tmp_leftover() {
        let dir = tempdir_for_test();
        let path = dir.join("settings.json");
        let mut store = SettingsStore::load_from_path(Some(path.clone()));
        store.modify(|s| {
            s.dark = true;
            s.paradigm = Some("Test".into());
        });

        let contents = fs::read_to_string(&path).unwrap();
        let reparsed: Settings = serde_json::from_str(&contents).expect("saved file must be valid JSON");
        assert!(reparsed.dark);
        assert_eq!(reparsed.paradigm.as_deref(), Some("Test"));

        let mut tmp_name = path.as_os_str().to_owned();
        tmp_name.push(".tmp");
        assert!(
            !std::path::Path::new(&tmp_name).exists(),
            "temp file must be cleaned up after successful save"
        );
    }

    /// A parse failure must rename the original to .corrupt, return defaults,
    /// and keep saves enabled (the next write creates a fresh file).
    #[test]
    fn parse_failure_preserves_original_as_backup_and_returns_defaults() {
        let dir = tempdir_for_test();
        let path = dir.join("settings.json");
        fs::write(&path, b"not valid json {{{").unwrap();

        let store = SettingsStore::load_from_path(Some(path.clone()));

        // Defaults must be in effect.
        assert!(!store.settings().dark);
        assert!(store.settings().recent_servers.is_empty());

        // Original must have been moved to the backup path.
        let mut backup_name = path.as_os_str().to_owned();
        backup_name.push(".corrupt");
        let backup = PathBuf::from(backup_name);
        assert!(backup.exists(), "corrupt backup must exist at settings.json.corrupt");
        assert_eq!(
            fs::read(&backup).unwrap(),
            b"not valid json {{{",
            "backup must contain the original bytes verbatim"
        );
        assert!(!path.exists(), "original must have been renamed to backup");

        // After backup, saves must be re-enabled (path is still Some).
        assert!(
            store.path().is_some(),
            "path must remain active after successful backup"
        );
    }

    fn tempdir_for_test() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "rumble-desktop-shell-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
