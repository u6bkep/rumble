//! Settings dialog — tab-based configuration UI for the damascene client.
//!
//! Owns its own ephemeral UI state (selected tab, pending values for
//! each section, which dropdown is open). The App routes events here
//! via [`handle_event`] and dispatches the resulting [`SettingsOutcome`]
//! back to the backend / `SettingsStore` / identity wizard.
//!
//! Save / Apply semantics mirror the rumble-egui dialog: most edits accumulate
//! in `pending_*` fields and only land when the user clicks Save or Apply. A few
//! controls (Refresh devices, Reset stats, Preview sfx, Regenerate
//! identity) fire immediately because they're side-effecting actions
//! rather than persisted state.

use std::path::PathBuf;

use damascene_core::{Color, prelude::*};

use rumble_client::{
    AudioSettings, AudioState, AudioStats, Level, MeterSnapshot, OutputFrame, OutputKind, OutputLayout, OutputSpec,
    PipelineConfig, ProcessorConfig, ProcessorRegistry, Role, SfxKind, State,
};
use rumble_desktop_shell::{
    AutoDownloadRule, HotkeyBinding, HotkeyData, HotkeyFunction, HotkeyManager, HotkeyModifiers, KeyboardSettings,
    PersistentVoiceMode, Settings, ShortcutEntry, TimestampFormat,
};
use rumble_protocol::permissions::Permissions;
use serde_json::Value as JsonValue;

use crate::{
    admin::{self, AdminOutcome, AdminState},
    identity::Identity,
};

// ============================================================
// State
// ============================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    Connection,
    Devices,
    Voice,
    Processing,
    Sounds,
    Chat,
    Shortcuts,
    Files,
    Stats,
    /// Server-administration UI. Rendered only when the connected user
    /// has `MANAGE_ACL` at the root; the tab disappears from the row
    /// when the permission is revoked mid-session.
    Admin,
    /// Build / version info — git describe output from build.rs.
    About,
}

impl SettingsTab {
    pub const ALL: &'static [SettingsTab] = &[
        SettingsTab::Connection,
        SettingsTab::Devices,
        SettingsTab::Voice,
        SettingsTab::Processing,
        SettingsTab::Sounds,
        SettingsTab::Chat,
        SettingsTab::Shortcuts,
        SettingsTab::Files,
        SettingsTab::Stats,
        SettingsTab::Admin,
        SettingsTab::About,
    ];

    fn slug(self) -> &'static str {
        match self {
            SettingsTab::Connection => "connection",
            SettingsTab::Devices => "devices",
            SettingsTab::Voice => "voice",
            SettingsTab::Processing => "processing",
            SettingsTab::Sounds => "sounds",
            SettingsTab::Chat => "chat",
            SettingsTab::Shortcuts => "shortcuts",
            SettingsTab::Files => "files",
            SettingsTab::Stats => "stats",
            SettingsTab::Admin => "admin",
            SettingsTab::About => "about",
        }
    }

    fn label(self) -> &'static str {
        match self {
            SettingsTab::Connection => "Connection",
            SettingsTab::Devices => "Devices",
            SettingsTab::Voice => "Voice",
            SettingsTab::Processing => "Processing",
            SettingsTab::Sounds => "Sounds",
            SettingsTab::Chat => "Chat",
            SettingsTab::Shortcuts => "Shortcuts",
            SettingsTab::Files => "Files",
            SettingsTab::Stats => "Stats",
            SettingsTab::Admin => "Admin",
            SettingsTab::About => "About",
        }
    }

    fn from_slug(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|t| t.slug() == s)
    }
}

/// True when the current user holds `MANAGE_ACL` on the root, gating
/// the Admin tab. Read on every render so revoking the permission
/// mid-session hides the tab immediately.
pub fn admin_visible(app_state: &State) -> bool {
    Permissions::from_bits_truncate(app_state.effective_permissions).contains(Permissions::MANAGE_ACL)
}

/// Filter `SettingsTab::ALL` to the tabs visible for the current
/// session — Admin is dropped when the user lacks `MANAGE_ACL`.
fn visible_tabs(app_state: &State) -> Vec<SettingsTab> {
    SettingsTab::ALL
        .iter()
        .copied()
        .filter(|t| !matches!(t, SettingsTab::Admin) || admin_visible(app_state))
        .collect()
}

/// At most one select dropdown is open at a time. Tracking it here
/// avoids one bool per select and gives `handle_event` a single place
/// to clear it when the user opens a different one.
///
/// The shortcuts table carries multiple per-row dropdowns (Function and
/// Data per shortcut entry), so those variants tag themselves with the
/// `ShortcutEntry::id` of the row they belong to.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum OpenSelect {
    #[default]
    None,
    InputDevice,
    OutputDevice,
    TimestampFormat,
    /// "Add processor" dropdown on the Processing tab.
    AddProcessor,
    /// Function dropdown open on the shortcut entry with this id.
    ShortcutFunction(String),
    /// Data dropdown open on the shortcut entry with this id.
    ShortcutData(String),
}

/// Pending edits accumulated while the dialog is open. Initialised
/// from the live values in [`SettingsState::open_with`] and read back
/// by [`SettingsOutcome::Save`].
#[derive(Debug, Clone, PartialEq)]
pub struct PendingSettings {
    // Connection
    pub autoconnect: bool,

    // Devices — `None` means "system default"; the outer Option tracks
    // whether the user touched the field at all.
    pub input_device: Option<String>,
    pub output_device: Option<String>,

    // Voice
    pub voice_mode: PersistentVoiceMode,
    pub audio: AudioSettings,

    // Processing — TX pipeline configuration. Seeded from
    // `state.audio.tx_pipeline` when the dialog opens.
    pub tx_pipeline: PipelineConfig,

    // Sounds
    pub sfx_enabled: bool,
    pub sfx_volume: f32,
    /// Indexed by `SfxKind::all()` order.
    pub sfx_kind_enabled: Vec<bool>,

    // Chat
    pub show_timestamps: bool,
    pub timestamp_format: TimestampFormat,
    pub auto_sync_history: bool,
    pub gif_autoplay: bool,

    // Shortcuts (keyboard) — full snapshot of the keyboard settings,
    // edited in place by the Shortcuts tab. Persisted as a whole on
    // Save and immediately re-registered with the HotkeyManager so
    // changes take effect live (the user can press the new key without
    // closing the dialog).
    pub keyboard: KeyboardSettings,

    // Files
    pub auto_download_enabled: bool,
    /// Editable rule list. The size column is held as a string
    /// (megabytes) so the user can clear the field while typing without
    /// it snapping back to `0`; we parse on Save.
    pub auto_download_rules: Vec<PendingAutoDownloadRule>,
    pub download_speed_kbps: u32,
    pub upload_speed_kbps: u32,
    /// Override for the downloads destination. `None` = platform
    /// default (system temp dir + `rumble_downloads`). Applied on
    /// next connect.
    pub download_dir: Option<PathBuf>,
}

/// One row of the auto-download rules editor.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PendingAutoDownloadRule {
    pub mime_pattern: String,
    /// Megabytes as user-typed text. Parsed back to `u64` bytes on Save;
    /// non-numeric / empty values become `0`, which matches
    /// [`AutoDownloadRule`]'s "rule disabled" sentinel.
    pub size_mb_text: String,
}

const MB: u64 = 1024 * 1024;
/// Default size for a freshly-added rule, mirroring the egui client's
/// 10 MB starting point.
const DEFAULT_RULE_SIZE_MB: u64 = 10;
/// Cap "Add Rule" can produce per click. The egui client lets users
/// type up to 1000 MB; we accept anything they paste here, but new
/// rules start under that.
const MAX_RULE_SIZE_MB: u64 = 1000;

impl PendingAutoDownloadRule {
    fn from_rule(rule: &AutoDownloadRule) -> Self {
        Self {
            mime_pattern: rule.mime_pattern.clone(),
            size_mb_text: (rule.max_size_bytes / MB).to_string(),
        }
    }

    /// Convert the editable row back to an [`AutoDownloadRule`]. An
    /// unparseable / empty size field becomes `0`, which the rule
    /// engine treats as "disabled" — preferable to dropping the row
    /// entirely, since the user can clear the field while editing.
    pub fn to_rule(&self) -> AutoDownloadRule {
        let mb: u64 = self.size_mb_text.trim().parse().unwrap_or(0);
        AutoDownloadRule {
            mime_pattern: self.mime_pattern.trim().to_string(),
            max_size_bytes: mb.saturating_mul(MB),
        }
    }
}

impl PendingSettings {
    fn from_live(audio: &AudioState, settings: &Settings) -> Self {
        let sfx_kind_enabled: Vec<bool> = SfxKind::all()
            .iter()
            .map(|k| !settings.sfx.disabled_sounds.contains(k))
            .collect();
        Self {
            autoconnect: settings.auto_connect_addr.is_some(),
            input_device: audio.selected_input.clone(),
            output_device: audio.selected_output.clone(),
            voice_mode: (&audio.voice_mode).into(),
            audio: audio.settings.clone(),
            tx_pipeline: audio.tx_pipeline.clone(),
            keyboard: settings.keyboard.clone(),
            sfx_enabled: settings.sfx.enabled,
            sfx_volume: settings.sfx.volume,
            sfx_kind_enabled,
            show_timestamps: settings.chat.show_timestamps,
            timestamp_format: settings.chat.timestamp_format,
            auto_sync_history: settings.chat.auto_sync_history,
            gif_autoplay: settings.chat.gif_autoplay,
            auto_download_enabled: settings.file_transfer.auto_download_enabled,
            auto_download_rules: settings
                .file_transfer
                .auto_download_rules
                .iter()
                .map(PendingAutoDownloadRule::from_rule)
                .collect(),
            download_speed_kbps: (settings.file_transfer.download_speed_limit / 1024) as u32,
            upload_speed_kbps: (settings.file_transfer.upload_speed_limit / 1024) as u32,
            download_dir: settings.file_transfer.download_dir.clone(),
        }
    }
}

#[derive(Debug, Default)]
pub struct SettingsState {
    pub open: bool,
    pub tab: Option<SettingsTab>,
    pub open_select: OpenSelect,
    pub pending: Option<PendingSettings>,
    /// Snapshot of the last settings state that was opened or applied.
    /// Used to disable Apply when the current pending edits match what
    /// has already landed.
    applied: Option<PendingSettings>,
    /// Persistent admin-tab UI state. Lives here so search query / open
    /// accordion / open popover survive across re-opens of the Settings
    /// dialog within one session.
    pub admin: AdminState,
    /// Entry ID currently capturing a keystroke. While set, the next
    /// `KeyDown` populates that entry's binding (and clears this).
    /// Cleared on Escape, tab switch, dialog close, or after a key fires.
    pub shortcut_capture: Option<String>,
}

impl SettingsState {
    /// Snapshot the live settings into pending state and show the
    /// dialog. Defaults the active tab to Connection.
    pub fn open_with(&mut self, audio: &AudioState, settings: &Settings) {
        let pending = PendingSettings::from_live(audio, settings);
        self.open = true;
        self.tab = Some(SettingsTab::Connection);
        self.open_select = OpenSelect::None;
        self.pending = Some(pending.clone());
        self.applied = Some(pending);
    }

    pub fn close(&mut self) {
        self.open = false;
        self.tab = None;
        self.open_select = OpenSelect::None;
        self.pending = None;
        self.applied = None;
        self.shortcut_capture = None;
    }

    /// Update the download-dir value while the dialog is still open.
    /// Called by the App after the async folder picker resolves; the
    /// dialog stays open with the new value waiting in pending state.
    pub fn set_pending_download_dir(&mut self, dir: Option<PathBuf>) {
        if let Some(pending) = self.pending.as_mut() {
            pending.download_dir = dir;
        }
    }

    fn has_pending_changes(&self) -> bool {
        match (&self.pending, &self.applied) {
            (Some(pending), Some(applied)) => pending != applied,
            (Some(_), None) => true,
            _ => false,
        }
    }

    fn mark_applied(&mut self) {
        self.applied = self.pending.clone();
    }
}

/// Resolve the effective download directory for a pending value.
/// `None` falls back to the same default the relay plugin would pick
/// at connect time, so an "Open" button always has somewhere to go.
pub fn effective_download_dir(pending: &PendingSettings) -> PathBuf {
    pending
        .download_dir
        .clone()
        .unwrap_or_else(|| std::env::temp_dir().join("rumble_downloads"))
}

/// Outcome of routing a single event into the settings dialog.
pub enum SettingsOutcome {
    Ignored,
    Handled,
    /// Close the dialog without saving.
    Close,
    /// Apply pending fields and close. The App reads the carried
    /// `PendingSettings` and writes it through to the backend +
    /// `SettingsStore`.
    Save(PendingSettings),
    /// Apply pending fields without closing the dialog.
    Apply(PendingSettings),
    /// User clicked "Generate new identity"; close settings and open
    /// the identity wizard at its method picker.
    OpenIdentityWizard,
    /// User clicked "Switch to SSH agent key…" / "Re-select SSH agent
    /// key…"; close settings and jump the wizard straight to the
    /// ssh-agent flow, skipping the method picker.
    OpenIdentityWizardAgent,
    /// User clicked "Elevate to superuser…"; close settings and open
    /// the sudo-password prompt.
    OpenElevate,
    /// User clicked "Copy" next to their public key. Carries the
    /// base64-encoded 32-byte Ed25519 pubkey for the App to push onto
    /// the OS clipboard.
    CopyPublicKey(String),
    /// One-shot side effects.
    PreviewSfx {
        kind: SfxKind,
        volume: f32,
    },
    RefreshDevices,
    ResetStats,
    /// Spawn the OS folder picker and update `download_dir` when
    /// the user picks a destination. Async; the App keeps the
    /// dialog open while the picker is showing.
    PickDownloadDir,
    /// Open `path` in the system file manager. Path is the
    /// effective destination (pending override, or platform default).
    OpenDownloadDir(PathBuf),
    /// Fire-and-forget commands surfaced by the Admin tab. Settings
    /// stays open so the user can keep editing.
    Dispatch(Vec<rumble_client::Command>),
    /// User edited the Shortcuts table — the new `KeyboardSettings`
    /// should be re-registered with the HotkeyManager so portal /
    /// global-hotkey bindings stay in sync with the pending edits.
    /// The App keeps the settings dialog open. Save still flushes to
    /// disk via the regular [`SettingsOutcome::Save`] path.
    RegisterHotkeys(KeyboardSettings),
    /// User clicked a shortcut cell on Wayland; open the compositor's
    /// configure-shortcuts dialog so the user can assign a key.
    OpenPortalShortcutSettings,
}

// ============================================================
// Routed-key constants
// ============================================================

const KEY_TABS: &str = "settings:tabs";
const KEY_DISMISS: &str = "settings:dismiss";
const KEY_CLOSE: &str = "settings:close";
const KEY_APPLY: &str = "settings:apply";
const KEY_SAVE: &str = "settings:save";

const KEY_AUTOCONNECT: &str = "settings:conn:autoconnect";
const KEY_REGENERATE: &str = "settings:conn:regenerate";
const KEY_SWITCH_AGENT: &str = "settings:conn:switch-agent";
const KEY_ELEVATE: &str = "settings:conn:elevate";
const KEY_COPY_PUBKEY: &str = "settings:conn:copy-pubkey";

const KEY_INPUT_DEVICE: &str = "settings:dev:input";
const KEY_OUTPUT_DEVICE: &str = "settings:dev:output";
const KEY_REFRESH_DEVICES: &str = "settings:dev:refresh";

const KEY_VOICE_MODE_TABS: &str = "settings:voice:mode";
const KEY_BITRATE_TABS: &str = "settings:voice:bitrate";
const KEY_VOICE_FEC: &str = "settings:voice:fec";
const KEY_VOICE_COMPLEXITY: &str = "settings:voice:complexity";
const KEY_VOICE_JITTER: &str = "settings:voice:jitter";
const KEY_VOICE_PACKET_LOSS: &str = "settings:voice:packet-loss";

const KEY_SFX_ENABLED: &str = "settings:sfx:enabled";
const KEY_SFX_VOLUME: &str = "settings:sfx:volume";

const KEY_CHAT_SHOW_TIMESTAMPS: &str = "settings:chat:show-timestamps";
const KEY_CHAT_FORMAT: &str = "settings:chat:format";
const KEY_CHAT_AUTO_SYNC: &str = "settings:chat:auto-sync";
const KEY_CHAT_GIF_AUTOPLAY: &str = "settings:chat:gif-autoplay";

/// Built-in auto-download rule presets shown next to "Add rule".
/// Order is the render order; pattern matches `mime_pattern_matches`
/// (suffix `/*` for top-level types, exact otherwise).
const RULE_PRESETS: &[(&str, &str, u64)] = &[
    ("Images", "image/*", 10),
    ("Audio", "audio/*", 25),
    ("Documents", "application/pdf", 5),
];
const KEY_FILES_PRESET_PREFIX: &str = "settings:files:preset:";
fn preset_key(idx: usize) -> String {
    format!("{KEY_FILES_PRESET_PREFIX}{idx}")
}
fn parse_preset_route(route: &str) -> Option<usize> {
    route.strip_prefix(KEY_FILES_PRESET_PREFIX).and_then(|s| s.parse().ok())
}

const KEY_FILES_AUTO_DOWNLOAD: &str = "settings:files:auto-download";
const KEY_FILES_DL_LIMIT: &str = "settings:files:download-limit";
const KEY_FILES_UL_LIMIT: &str = "settings:files:upload-limit";
const KEY_FILES_DIR_BROWSE: &str = "settings:files:dir:browse";
const KEY_FILES_DIR_OPEN: &str = "settings:files:dir:open";
const KEY_FILES_DIR_RESET: &str = "settings:files:dir:reset";

/// Auto-download rules editor. Rule rows use `<prefix>:<idx>:<field>`;
/// the shared prefix lets [`handle_event`] cheaply early-out for
/// unrelated routes.
const RULE_PREFIX: &str = "settings:files:rule:";
const KEY_FILES_RULE_ADD: &str = "settings:files:rule:add";

fn rule_mime_key(idx: usize) -> String {
    format!("{RULE_PREFIX}{idx}:mime")
}
fn rule_size_key(idx: usize) -> String {
    format!("{RULE_PREFIX}{idx}:size")
}
fn rule_remove_key(idx: usize) -> String {
    format!("{RULE_PREFIX}{idx}:remove")
}

/// Inverse of the rule key constructors. Returns (idx, field) for any
/// `settings:files:rule:<idx>:<field>` route, or `None` for unrelated
/// routes (including `:add`, which is matched separately).
fn parse_rule_route(route: &str) -> Option<(usize, &str)> {
    let rest = route.strip_prefix(RULE_PREFIX)?;
    let (idx_str, field) = rest.split_once(':')?;
    let idx: usize = idx_str.parse().ok()?;
    Some((idx, field))
}

const KEY_STATS_RESET: &str = "settings:stats:reset";

// ---------- Shortcuts tab ----------

const KEY_SHORTCUTS_GLOBAL_ENABLE: &str = "settings:shortcuts:global-enable";
const KEY_SHORTCUTS_ADD: &str = "settings:shortcuts:add";
const SHORTCUTS_ROW_PREFIX: &str = "settings:shortcuts:row:";

fn shortcuts_row_key(entry_id: &str) -> String {
    format!("{SHORTCUTS_ROW_PREFIX}{entry_id}")
}
fn shortcut_function_key(entry_id: &str) -> String {
    format!("{SHORTCUTS_ROW_PREFIX}{entry_id}:function")
}
fn shortcut_data_key(entry_id: &str) -> String {
    format!("{SHORTCUTS_ROW_PREFIX}{entry_id}:data")
}
fn shortcut_binding_key(entry_id: &str) -> String {
    format!("{SHORTCUTS_ROW_PREFIX}{entry_id}:binding")
}
fn shortcut_remove_key(entry_id: &str) -> String {
    format!("{SHORTCUTS_ROW_PREFIX}{entry_id}:remove")
}

/// Inverse of the shortcut row key constructors. Returns
/// `(entry_id, field_path)` for any `settings:shortcuts:row:<id>:<field…>`
/// route. UUIDs contain dashes but no colons, so the first colon after
/// the prefix is always the entry-id/field boundary. `field_path` may
/// include further `:`-separated tail (e.g. `function:option:ptt`,
/// `data:dismiss`) — callers match on the first segment with
/// [`shortcut_field_kind`].
fn parse_shortcut_route(route: &str) -> Option<(&str, &str)> {
    let rest = route.strip_prefix(SHORTCUTS_ROW_PREFIX)?;
    rest.split_once(':')
}

/// First `:`-separated segment of a field path — the part that
/// distinguishes the function dropdown, data dropdown, binding button,
/// and bare row click.
fn shortcut_field_kind(field_path: &str) -> &str {
    field_path.split_once(':').map(|(head, _)| head).unwrap_or(field_path)
}

/// Processing-tab routed keys are built per-processor / per-field at
/// render time; see [`proc_enabled_key`] / [`proc_field_key`]. The
/// shared prefix lets [`handle_event`] cheaply ignore unrelated routes.
const PROC_PREFIX: &str = "settings:proc:";

/// Trigger for the "Add processor" dropdown at the bottom of the
/// Processing tab. Not parsed by [`parse_proc_route`] — handled with
/// `select::classify_event` like the other dropdowns.
const KEY_PROC_ADD: &str = "settings:proc-add";

/// Limits clamp KB/s sliders. 5000 KB/s ≈ 5 MB/s — well above any
/// realistic rumble file-transfer cap.
const MAX_SPEED_KBPS: u32 = 5000;

fn sfx_kind_key(idx: usize) -> String {
    format!("settings:sfx:kind:{idx}")
}
fn sfx_preview_key(idx: usize) -> String {
    format!("settings:sfx:preview:{idx}")
}

/// Route key for a processor's enable switch. The slot is identified by
/// the processor's index in the pipeline rather than its `type_id` so
/// keys stay short and the parsing in `handle_event` is a single
/// `parse::<usize>()`. Indices are stable within a render cycle; the
/// reorder / remove / add buttons mutate the underlying `Vec` *after*
/// the event has been handled, so the route → index mapping in flight
/// matches the tree that produced the event.
fn proc_enabled_key(idx: usize) -> String {
    format!("{PROC_PREFIX}{idx}:enabled")
}

/// Route key for one schema field of a processor. Layout:
/// `settings:proc:<idx>:f:<field>`. The `:f:` separator avoids
/// collisions with `enabled` / `up` / `down` / `remove` even if a
/// schema were ever to define a field literally named one of those.
fn proc_field_key(idx: usize, field: &str) -> String {
    format!("{PROC_PREFIX}{idx}:f:{field}")
}

fn proc_move_up_key(idx: usize) -> String {
    format!("{PROC_PREFIX}{idx}:up")
}
fn proc_move_down_key(idx: usize) -> String {
    format!("{PROC_PREFIX}{idx}:down")
}
fn proc_remove_key(idx: usize) -> String {
    format!("{PROC_PREFIX}{idx}:remove")
}

/// Per-processor route slots. Builders above produce the strings;
/// [`parse_proc_route`] inverts them back to (idx, slot).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcRouteSlot<'a> {
    Enabled,
    Field(&'a str),
    MoveUp,
    MoveDown,
    Remove,
}

/// Inverse of the `proc_*_key` helpers. Returns the
/// (processor index, slot) for routes under the processing namespace,
/// or `None` for unrelated routes (including [`KEY_PROC_ADD`], which is
/// matched separately).
fn parse_proc_route(route: &str) -> Option<(usize, ProcRouteSlot<'_>)> {
    let rest = route.strip_prefix(PROC_PREFIX)?;
    let (idx_str, tail) = rest.split_once(':')?;
    let idx: usize = idx_str.parse().ok()?;
    let slot = match tail {
        "enabled" => ProcRouteSlot::Enabled,
        "up" => ProcRouteSlot::MoveUp,
        "down" => ProcRouteSlot::MoveDown,
        "remove" => ProcRouteSlot::Remove,
        other => ProcRouteSlot::Field(other.strip_prefix("f:")?),
    };
    Some((idx, slot))
}

// ============================================================
// Render
// ============================================================

/// Render the settings dialog and any open dropdown popover. Returns
/// `(panel_layer, popover_layer)` so the App can compose them as
/// independent overlay layers — popovers must paint above the modal
/// panel they were anchored to. Returns `None` for the panel when the
/// dialog is closed.
#[allow(clippy::too_many_arguments)]
pub fn render(
    state: &SettingsState,
    app_state: &State,
    meter: &MeterSnapshot,
    stats: &AudioStats,
    outputs: &OutputFrame,
    identity: &Identity,
    selection: &Selection,
    processor_registry: &ProcessorRegistry,
    hotkeys: &HotkeyManager,
) -> (Option<El>, Option<El>) {
    if !state.open {
        return (None, None);
    }
    let pending = match &state.pending {
        Some(p) => p,
        None => return (None, None),
    };
    // Resolve the active tab against the live visibility filter so a
    // permission revocation mid-session falls back gracefully to
    // Connection instead of rendering a hidden tab.
    let tabs = visible_tabs(app_state);
    let mut tab = state.tab.unwrap_or(SettingsTab::Connection);
    if !tabs.contains(&tab) {
        tab = SettingsTab::Connection;
    }

    let body = match tab {
        SettingsTab::Connection => render_connection(pending, identity, app_state),
        SettingsTab::Devices => render_devices(pending, &app_state.audio, meter),
        SettingsTab::Voice => render_voice(pending),
        SettingsTab::Processing => render_processing(pending, meter, outputs, processor_registry, selection),
        SettingsTab::Sounds => render_sounds(pending),
        SettingsTab::Chat => render_chat(pending),
        SettingsTab::Shortcuts => render_shortcuts(pending, state, hotkeys),
        SettingsTab::Files => render_files(pending, selection),
        SettingsTab::Stats => render_stats(stats),
        SettingsTab::Admin => admin::render(&state.admin, app_state, selection),
        SettingsTab::About => render_about(),
    };

    // These tabs paint live audio data pulled fresh each frame
    // (`backend.meter()` / `.outputs()` / `.stats()`) rather than from
    // `State`, so a state-change wakeup won't refresh them. Ask the host
    // to keep ticking at ~30fps while one is on screen — the same
    // self-scheduling `redraw_within` pattern animated GIFs and the video
    // surface use. Every other tab is static and falls back to 0fps idle.
    let body = if matches!(tab, SettingsTab::Devices | SettingsTab::Processing | SettingsTab::Stats) {
        body.redraw_within(std::time::Duration::from_millis(33))
    } else {
        body
    };

    // Vertical nav rail. Reuses the same routed-key format as
    // `tabs_list` (`{KEY_TABS}:tab:{slug}`) so `tabs::apply_event` in
    // `handle_event` still routes selections — only the visual shell
    // changes from a segmented pill to a sidebar.
    let nav = sidebar(
        tabs.iter()
            .map(|t| sidebar_menu_button(t.label(), *t == tab).key(tab_option_key(KEY_TABS, &t.slug()))),
    )
    .width(Size::Fixed(196.0))
    .height(Size::Fill(1.0))
    .padding(Sides::all(tokens::SPACE_2))
    .gap(tokens::SPACE_1);

    let mut apply_button = button("Apply").key(KEY_APPLY).secondary();
    if !state.has_pending_changes() {
        apply_button = apply_button.disabled();
    }

    let footer = row([
        button("Close").key(KEY_CLOSE),
        spacer(),
        apply_button,
        button("Save").key(KEY_SAVE).primary(),
    ])
    .gap(tokens::SPACE_2)
    .width(Size::Fill(1.0))
    .align(Align::Center);

    // Sidebar nav decouples the dialog width from the tab count, so
    // the frame stays a fixed size regardless of how many sections we
    // add. 960 px gives the nav rail ~196 px plus a comfortable
    // content column for the widest tabs (Shortcuts, Processing).
    let panel = modal_panel(
        "Settings",
        [
            row([
                nav,
                // Horizontal padding has to live on a wrapper *inside* the
                // scroll, not on the scroll itself: the scrollbar thumb is
                // anchored to `scroll.inner.right() - 8`, so padding on
                // scroll moves the thumb and content together and the
                // thumb keeps painting on top of full-width children.
                // Wrapping the body lets content stop short of the
                // scrollbar gutter (active thumb = 10 px + 2 px track
                // inset = 12 px).
                scroll([column([body])
                    .padding(Sides::xy(tokens::SPACE_3, 0.0))
                    .width(Size::Fill(1.0))])
                .padding(Sides::xy(0.0, tokens::SPACE_2))
                .gap(tokens::SPACE_3)
                .width(Size::Fill(1.0))
                .height(Size::Fill(1.0)),
            ])
            .gap(tokens::SPACE_3)
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0))
            .align(Align::Stretch),
            divider(),
            footer,
        ],
    )
    .width(Size::Fixed(960.0))
    .height(Size::Fixed(620.0));

    let panel_layer = overlay([scrim(KEY_DISMISS), panel.block_pointer()]);

    let select_popover = match &state.open_select {
        OpenSelect::None => None,
        OpenSelect::InputDevice => Some(select_menu(
            KEY_INPUT_DEVICE,
            std::iter::once(("__default".to_string(), "System default".to_string())).chain(
                app_state
                    .audio
                    .input_devices
                    .iter()
                    .map(|d| (d.id.clone(), d.name.clone())),
            ),
        )),
        OpenSelect::OutputDevice => Some(select_menu(
            KEY_OUTPUT_DEVICE,
            std::iter::once(("__default".to_string(), "System default".to_string())).chain(
                app_state
                    .audio
                    .output_devices
                    .iter()
                    .map(|d| (d.id.clone(), d.name.clone())),
            ),
        )),
        OpenSelect::TimestampFormat => Some(select_menu(
            KEY_CHAT_FORMAT,
            TimestampFormat::ALL
                .iter()
                .enumerate()
                .map(|(idx, fmt)| (idx.to_string(), fmt.label().to_string())),
        )),
        OpenSelect::AddProcessor => Some(select_menu(
            KEY_PROC_ADD,
            processor_registry
                .list_available()
                .into_iter()
                .map(|(type_id, name, _desc)| (type_id.to_string(), name.to_string())),
        )),
        OpenSelect::ShortcutFunction(entry_id) => Some(select_menu(
            shortcut_function_key(entry_id),
            HotkeyFunction::ALL
                .iter()
                .map(|f| (f.slug().to_string(), f.label().to_string())),
        )),
        OpenSelect::ShortcutData(entry_id) => {
            let entry = pending.keyboard.shortcuts.iter().find(|e| e.id == *entry_id);
            let options = entry
                .map(|e| e.function.data_options())
                .unwrap_or(&[HotkeyData::Toggle]);
            Some(select_menu(
                shortcut_data_key(entry_id),
                options.iter().map(|d| (d.slug().to_string(), d.label().to_string())),
            ))
        }
    };

    // Admin overlays (action menus, popovers, dialogs) ride above the
    // settings panel. Only meaningful while the Admin tab is active —
    // closing the tab leaves the state in place but the overlays are
    // suppressed to avoid floating orphans.
    let popover_layer = if matches!(tab, SettingsTab::Admin) {
        let admin_overlays = admin::render_overlays(&state.admin, app_state, selection);
        if admin_overlays.any() {
            // Stack admin overlays + the select dropdown into one
            // layer using `overlays(...)`; the empty main is just a
            // pass-through so the stack composes flat.
            let main = stack::<_, El>([]);
            let layers: Vec<Option<El>> = admin_overlays
                .into_layers()
                .into_iter()
                .chain(std::iter::once(select_popover))
                .collect();
            Some(overlays(main, layers))
        } else {
            select_popover
        }
    } else {
        select_popover
    };

    (Some(panel_layer), popover_layer)
}

// ---- per-tab views --------------------------------------------------

fn render_connection(pending: &PendingSettings, identity: &Identity, app_state: &State) -> El {
    use rumble_desktop_shell::KeySource;

    let mut cards: Vec<El> = Vec::new();

    let Some(config) = identity.manager().config().cloned() else {
        cards.push(section_card(
            "Identity",
            [
                paragraph("No identity configured.").text_color(tokens::WARNING),
                row([spacer(), button("Set up identity…").key(KEY_REGENERATE).primary()]).width(Size::Fill(1.0)),
            ],
        ));
        cards.push(section_card("Auto-connect", render_autoconnect_rows(pending)));
        if let Some(superuser) = render_superuser_section(app_state) {
            cards.push(section_card("Superuser", superuser));
        }
        return column(cards).gap(tokens::SPACE_3).width(Size::Fill(1.0));
    };

    // ---- Source detail ----
    let (storage_label, storage_detail) = match &config.source {
        KeySource::LocalPlaintext { .. } => (
            "Local (unencrypted)",
            "Stored unencrypted at identity.json — fine for personal machines.".to_string(),
        ),
        KeySource::LocalEncrypted { .. } => (
            "Local (password protected)",
            "Encrypted with Argon2 + ChaCha20-Poly1305. Password required at startup.".to_string(),
        ),
        KeySource::SshAgent {
            fingerprint: agent_fp,
            comment,
        } => {
            let detail = if comment.is_empty() {
                format!("Bound to ssh-agent key {agent_fp}.")
            } else {
                format!("Bound to ssh-agent key \"{comment}\" ({agent_fp}).")
            };
            ("SSH agent", detail)
        }
    };

    let mut identity_rows: Vec<El> = Vec::new();
    identity_rows.push(field_row("Storage", text(storage_label.to_string()).semibold()));
    identity_rows.push(paragraph(storage_detail).muted().font_size(tokens::TEXT_XS.size));

    // For ssh-agent identities: show whether the agent socket is
    // reachable right now. We can't verify the bound key is loaded
    // without an async call, but env-level reachability is the most
    // common failure mode (running Rumble outside a desktop session,
    // forgot to start the agent, etc.).
    let agent_reachable = ssh_agent_available();
    if matches!(config.source, KeySource::SshAgent { .. }) {
        if agent_reachable {
            identity_rows.push(
                alert([
                    alert_title("SSH agent reachable"),
                    alert_description(
                        "Rumble will ask the agent to sign on connect. The agent must still hold this key.",
                    ),
                ])
                .info(),
            );
        } else {
            identity_rows.push(
                alert([
                    alert_title("SSH agent not reachable"),
                    alert_description(
                        "SSH_AUTH_SOCK is unset in this environment. Signing will fail when you try to connect — \
                         start ssh-agent (or relaunch Rumble from a shell with SSH_AUTH_SOCK set).",
                    ),
                ])
                .warning(),
            );
        }
    }

    identity_rows.push(field_row(
        "Fingerprint",
        mono(identity.fingerprint())
            .font_size(tokens::TEXT_XS.size)
            .ellipsis()
            .width(Size::Fill(1.0)),
    ));
    if let Some(pubkey) = identity.public_key() {
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, pubkey);
        identity_rows.push(field_row(
            "Public key",
            row([
                mono(b64)
                    .font_size(tokens::TEXT_XS.size)
                    .ellipsis()
                    .width(Size::Fill(1.0)),
                button("Copy").key(KEY_COPY_PUBKEY),
            ])
            .gap(tokens::SPACE_2)
            .width(Size::Fill(1.0))
            .align(Align::Center),
        ));
    }
    let path = identity.manager().config_dir().join("identity.json");
    identity_rows.push(field_row(
        "On disk",
        mono(path.display().to_string())
            .font_size(tokens::TEXT_XS.size)
            .ellipsis()
            .width(Size::Fill(1.0)),
    ));

    // ---- Switch source (non-destructive) ----
    // For local sources: "Switch to SSH agent key…".
    // For agent sources: "Re-select SSH agent key…" (re-pick from agent;
    // the local identity file just gets rewritten with the new
    // fingerprint, no key material is destroyed).
    let agent_btn_label = match &config.source {
        KeySource::SshAgent { .. } => "Re-select SSH agent key…",
        _ => "Switch to SSH agent key…",
    };
    let agent_btn = button(agent_btn_label).key(KEY_SWITCH_AGENT).secondary();
    let agent_btn = if agent_reachable {
        agent_btn
    } else {
        agent_btn.disabled()
    };
    identity_rows.push(row([spacer(), agent_btn]).width(Size::Fill(1.0)));

    cards.push(section_card("Identity", identity_rows));

    cards.push(section_card(
        "Replace identity",
        [
            alert([
                alert_title("Regenerating overwrites your identity"),
                alert_description(
                    "Servers that knew the old key won't recognise the new one — you'll have to re-register or be \
                     re-approved.",
                ),
            ])
            .warning(),
            row([
                spacer(),
                button("Generate new identity…").key(KEY_REGENERATE).destructive(),
            ])
            .width(Size::Fill(1.0)),
        ],
    ));

    cards.push(section_card("Auto-connect", render_autoconnect_rows(pending)));

    if let Some(superuser) = render_superuser_section(app_state) {
        cards.push(section_card("Superuser", superuser));
    }

    column(cards).gap(tokens::SPACE_3).width(Size::Fill(1.0))
}

fn render_autoconnect_rows(pending: &PendingSettings) -> Vec<El> {
    vec![
        field_row(
            "Autoconnect on launch",
            switch(pending.autoconnect).key(KEY_AUTOCONNECT),
        ),
        paragraph(
            "Reconnects to the most recently used server on startup. Effective once you've connected to at least one \
             server.",
        )
        .muted()
        .font_size(tokens::TEXT_XS.size),
    ]
}

/// True when the ssh-agent socket appears reachable from this process.
/// Mirrors wizard.rs's check; sync because it's a bare env-var test.
fn ssh_agent_available() -> bool {
    rumble_desktop_shell::identity::SshAgentClient::is_available()
}

/// Render the Elevate / superuser section of the Connection tab, or
/// `None` when the local user can't sudo (not connected, or the
/// server's effective permissions don't carry [`Permissions::SUDO`]).
/// When already elevated, swaps the button for a status notice.
fn render_superuser_section(app_state: &State) -> Option<Vec<El>> {
    if !app_state.connection.is_connected() {
        return None;
    }
    let perms = Permissions::from_bits_truncate(app_state.effective_permissions);
    if !perms.contains(Permissions::SUDO) {
        return None;
    }
    let already_elevated = app_state
        .my_user_id
        .and_then(|id| app_state.get_user(id))
        .map(|u| u.is_elevated)
        .unwrap_or(false);

    let mut out: Vec<El> = Vec::new();
    if already_elevated {
        out.push(
            alert([
                alert_title("Elevated this session"),
                alert_description("You're operating as superuser. Disconnect to drop the elevation."),
            ])
            .info(),
        );
    } else {
        out.push(
            paragraph(
                "Elevation bypasses the ACL system for the rest of this session — disconnect to drop it. Requires the \
                 server-side sudo password set via `server set-sudo-password`.",
            )
            .muted()
            .font_size(tokens::TEXT_XS.size),
        );
        out.push(row([spacer(), button("Elevate to superuser…").key(KEY_ELEVATE).primary()]).width(Size::Fill(1.0)));
    }
    Some(out)
}

fn render_devices(pending: &PendingSettings, audio: &AudioState, meter: &MeterSnapshot) -> El {
    let input_label = device_label_for(pending.input_device.as_deref(), &audio.input_devices);
    let output_label = device_label_for(pending.output_device.as_deref(), &audio.output_devices);

    column([
        section_card(
            "Input device",
            [
                select_trigger(KEY_INPUT_DEVICE, input_label).width(Size::Fill(1.0)),
                // Pre-pipeline level: lets the user confirm the selected
                // mic is actually capturing before they ever open the
                // Processing tab. No VAD overlay here — the threshold
                // belongs to the processing chain, not to device choice.
                vu_meter(meter.input_pre, None),
            ],
        ),
        section_card(
            "Output device",
            [select_trigger(KEY_OUTPUT_DEVICE, output_label).width(Size::Fill(1.0))],
        ),
        section_card(
            "Device list",
            [
                row([button("Refresh devices").key(KEY_REFRESH_DEVICES).secondary(), spacer()]).width(Size::Fill(1.0)),
                paragraph(
                    "Device changes apply when you click Save. Switching devices while connected may briefly drop \
                     audio.",
                )
                .muted()
                .font_size(tokens::TEXT_XS.size),
            ],
        ),
    ])
    .gap(tokens::SPACE_3)
    .width(Size::Fill(1.0))
}

fn render_voice(pending: &PendingSettings) -> El {
    let mode_slug = match pending.voice_mode {
        PersistentVoiceMode::PushToTalk => "ptt",
        PersistentVoiceMode::Continuous => "cont",
    };
    let bitrate_slug = bitrate_slug(pending.audio.bitrate);

    column([
        section_card(
            "Voice mode",
            [
                tabs_list(
                    KEY_VOICE_MODE_TABS,
                    &mode_slug,
                    [("ptt", "Push-to-Talk"), ("cont", "Continuous")],
                ),
                paragraph(match pending.voice_mode {
                    PersistentVoiceMode::PushToTalk => "Hold the configured PTT key (default: Space) to transmit.",
                    PersistentVoiceMode::Continuous => {
                        "Always transmitting. Add a VAD processor to gate on voice activity."
                    }
                })
                .muted()
                .font_size(tokens::TEXT_XS.size),
            ],
        ),
        section_card(
            "Encoder",
            [
                field_row(
                    "Bitrate",
                    tabs_list(
                        KEY_BITRATE_TABS,
                        &bitrate_slug,
                        [
                            ("low", "24 kbps"),
                            ("medium", "32 kbps"),
                            ("high", "64 kbps"),
                            ("very-high", "96 kbps"),
                        ],
                    )
                    .width(Size::Fixed(360.0)),
                ),
                field_row(
                    format!("Complexity ({})", pending.audio.encoder_complexity),
                    slider(pending.audio.encoder_complexity as f32 / 10.0, tokens::PRIMARY)
                        .key(KEY_VOICE_COMPLEXITY)
                        .width(Size::Fixed(280.0)),
                ),
                paragraph("Higher complexity = better quality, more CPU. Range 0–10.")
                    .muted()
                    .font_size(tokens::TEXT_XS.size),
            ],
        ),
        section_card(
            "Network",
            [
                field_row(
                    "Forward Error Correction",
                    switch(pending.audio.fec_enabled).key(KEY_VOICE_FEC),
                ),
                field_row(
                    format!(
                        "Jitter buffer ({} packets · ~{}ms)",
                        pending.audio.jitter_buffer_delay_packets,
                        pending.audio.jitter_buffer_delay_packets * 20
                    ),
                    slider(
                        (pending.audio.jitter_buffer_delay_packets as f32 - 1.0) / 9.0,
                        tokens::PRIMARY,
                    )
                    .key(KEY_VOICE_JITTER)
                    .width(Size::Fixed(280.0)),
                ),
                field_row(
                    format!("Expected packet loss ({}%)", pending.audio.packet_loss_percent),
                    slider(pending.audio.packet_loss_percent as f32 / 25.0, tokens::PRIMARY)
                        .key(KEY_VOICE_PACKET_LOSS)
                        .width(Size::Fixed(280.0)),
                ),
            ],
        ),
    ])
    .gap(tokens::SPACE_3)
    .width(Size::Fill(1.0))
}

fn render_processing(
    pending: &PendingSettings,
    meter: &MeterSnapshot,
    outputs: &OutputFrame,
    registry: &ProcessorRegistry,
    selection: &Selection,
) -> El {
    let mut cards: Vec<El> = Vec::new();
    let processor_count = pending.tx_pipeline.processors.len();
    // Slot assignment for live stage outputs, derived from the same config
    // the audio task derives from — so `outputs.values[slot]` lines up.
    let output_layout = OutputLayout::derive(&pending.tx_pipeline, registry);
    // Look up display metadata once per render. `list_available` walks
    // the registry's BTreeMap and allocates a Vec, so doing it inside
    // the per-processor loop (as the previous code did) was O(n * m).
    let available: Vec<(String, String, String)> = registry
        .list_available()
        .into_iter()
        .map(|(type_id, name, desc)| (type_id.to_string(), name.to_string(), desc.to_string()))
        .collect();

    cards.push(section_card(
        "Processor pipeline",
        [paragraph(
            "Audio processors applied to your microphone before encoding. Drag stages with the arrow buttons to \
             change order, add or remove stages with the controls below.",
        )
        .muted()
        .font_size(tokens::TEXT_XS.size)],
    ));

    if processor_count == 0 {
        cards.push(section_card(
            "Processors",
            [paragraph("No processors in the pipeline. Add one below to get started.").muted()],
        ));
    }

    for (idx, proc_config) in pending.tx_pipeline.processors.iter().enumerate() {
        let (display_name, description) = available
            .iter()
            .find(|(type_id, _, _)| type_id == &proc_config.type_id)
            .map(|(_, name, desc)| (name.clone(), desc.clone()))
            .unwrap_or_else(|| (proc_config.type_id.clone(), String::new()));

        let is_first = idx == 0;
        let is_last = idx + 1 == processor_count;

        let mut move_up_btn = icon_button(IconName::ChevronUp)
            .key(proc_move_up_key(idx))
            .ghost()
            .tooltip("Move up");
        if is_first {
            move_up_btn = move_up_btn.disabled();
        }
        let mut move_down_btn = icon_button(IconName::ChevronDown)
            .key(proc_move_down_key(idx))
            .ghost()
            .tooltip("Move down");
        if is_last {
            move_down_btn = move_down_btn.disabled();
        }
        let remove_btn = icon_button(IconName::X)
            .key(proc_remove_key(idx))
            .ghost()
            .tooltip("Remove from pipeline");

        // Header row: action buttons on the left, enable switch flush
        // right. The `spacer` separates the two clusters and absorbs any
        // extra width as the dialog resizes.
        let mut body: Vec<El> = Vec::new();
        body.push(
            row([
                move_up_btn,
                move_down_btn,
                remove_btn,
                spacer(),
                switch(proc_config.enabled).key(proc_enabled_key(idx)),
            ])
            .gap(tokens::SPACE_2)
            .align(Align::Center)
            .width(Size::Fill(1.0)),
        );
        if !description.is_empty() {
            body.push(paragraph(description).muted().font_size(tokens::TEXT_XS.size));
        }
        if proc_config.enabled
            && let Some(schema) = registry.settings_schema(&proc_config.type_id)
            && let Some(properties) = schema.get("properties").and_then(|p| p.as_object())
        {
            for (key, prop_schema) in properties {
                body.push(render_schema_field(
                    idx,
                    key,
                    prop_schema,
                    &proc_config.settings,
                    selection,
                ));
            }
        }

        // Live output feedback (VAD probability meter, gate lamp). Driven
        // entirely by the stage's declared output specs — no per-processor
        // code here. Resolved against the live `outputs` frame by slot.
        if proc_config.enabled {
            for entry in output_layout.for_processor(idx) {
                let value = outputs.get(entry.slot).unwrap_or(0.0);
                body.push(render_output(&entry.spec, value, &proc_config.settings));
            }
        }

        cards.push(section_card(display_name, body));
    }

    // "Add processor" card sits at the bottom of the list so the action
    // is visible without scrolling past meters. The trigger always shows
    // the same label — picking an option fires Add and the dropdown
    // closes; there is no "selected" state to display.
    cards.push(section_card(
        "Add processor",
        [row([
            select_trigger(KEY_PROC_ADD, "Add a processor…").width(Size::Fixed(280.0)),
            spacer(),
        ])
        .gap(tokens::SPACE_2)
        .align(Align::Center)
        .width(Size::Fill(1.0))],
    ));

    cards.push(section_card(
        "Transmit level",
        [vu_meter(meter.input_post, Some(&pending.tx_pipeline))],
    ));

    column(cards).gap(tokens::SPACE_3).width(Size::Fill(1.0))
}

/// Render one schema-defined property as a labelled control. `number` /
/// `integer` types become sliders (the only general-purpose continuous
/// control damascene ships out of the box), `boolean` becomes a switch,
/// everything else falls back to a text input.
fn render_schema_field(
    proc_idx: usize,
    key: &str,
    schema: &JsonValue,
    settings: &JsonValue,
    selection: &Selection,
) -> El {
    let title = schema.get("title").and_then(|t| t.as_str()).unwrap_or(key).to_string();
    let prop_type = schema.get("type").and_then(|t| t.as_str()).unwrap_or("string");
    let route = proc_field_key(proc_idx, key);

    match prop_type {
        "number" => {
            let min = schema.get("minimum").and_then(|m| m.as_f64()).unwrap_or(-100.0) as f32;
            let max = schema.get("maximum").and_then(|m| m.as_f64()).unwrap_or(100.0) as f32;
            let default = schema.get("default").and_then(|d| d.as_f64()).unwrap_or(0.0) as f32;
            let value = settings
                .get(key)
                .and_then(|v| v.as_f64())
                .map(|v| v as f32)
                .unwrap_or(default);
            let normalized = if (max - min).abs() < f32::EPSILON {
                0.0
            } else {
                ((value - min) / (max - min)).clamp(0.0, 1.0)
            };
            field_row(
                format!("{title} ({:.2})", value),
                slider(normalized, tokens::PRIMARY)
                    .key(&route)
                    .width(Size::Fixed(220.0)),
            )
        }
        "integer" => {
            let min = schema.get("minimum").and_then(|m| m.as_i64()).unwrap_or(0);
            let max = schema.get("maximum").and_then(|m| m.as_i64()).unwrap_or(100);
            let default = schema.get("default").and_then(|d| d.as_i64()).unwrap_or(0);
            let value = settings.get(key).and_then(|v| v.as_i64()).unwrap_or(default);
            let span = (max - min).max(1) as f32;
            let normalized = ((value - min) as f32 / span).clamp(0.0, 1.0);
            field_row(
                format!("{title} ({value})"),
                slider(normalized, tokens::PRIMARY)
                    .key(&route)
                    .width(Size::Fixed(220.0)),
            )
        }
        "boolean" => {
            let default = schema.get("default").and_then(|d| d.as_bool()).unwrap_or(false);
            let value = settings.get(key).and_then(|v| v.as_bool()).unwrap_or(default);
            field_row(title, switch(value).key(&route))
        }
        _ => {
            let default = schema.get("default").and_then(|d| d.as_str()).unwrap_or("");
            let value = settings
                .get(key)
                .and_then(|v| v.as_str())
                .unwrap_or(default)
                .to_string();
            field_row(title, text_input(&value, selection, &route).width(Size::Fixed(220.0)))
        }
    }
}

fn bool_setting(proc: &ProcessorConfig, key: &str) -> bool {
    proc.settings.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

/// Dynamic range for the meter, in dB. -60 dB is roughly the floor for
/// "audible" speech; 0 dB is digital full-scale. The bar normalises
/// `level_db` linearly across this span.
const VU_MIN_DB: f32 = -60.0;
const VU_MAX_DB: f32 = 0.0;
const VU_SPAN_DB: f32 = VU_MAX_DB - VU_MIN_DB;
const VU_BAR_W: f32 = 280.0;
const VU_BAR_H: f32 = 14.0;

fn vu_normalize(db: f32) -> f32 {
    ((db - VU_MIN_DB) / VU_SPAN_DB).clamp(0.0, 1.0)
}

/// Shared VU meter used by both the Devices and Processing tabs.
///
/// `level` is the value to render — Devices passes `meter.input_pre`,
/// Processing passes `meter.input_post`. `Level::Unmeasured` paints an
/// empty bar and shows "—" for the readout.
///
/// `vad_overlay` controls the Mumble-style split colouring. When `Some`,
/// the meter probes the pipeline config for an enabled `builtin.vad`
/// processor and, if found, paints "won't transmit" / "will transmit"
/// zones either side of its threshold. When the VAD processor isn't
/// present (or `vad_overlay` is `None`) the meter falls back to a plain
/// green/yellow/red loudness display.
///
/// This is the only place the meter's visual contract is defined —
/// callers just pass a level and decide whether the overlay applies.
fn vu_meter(level: Level, vad_overlay: Option<&PipelineConfig>) -> El {
    let level_db = level.as_db();
    let value_n = level.normalized(VU_MIN_DB, VU_MAX_DB);

    // Resolve VAD overlay state. With no pipeline provided (Devices
    // tab), or no enabled VAD processor in it, the meter degrades to
    // plain loudness — that's the whole point of letting the caller
    // opt out of the overlay.
    let vad_proc = vad_overlay.and_then(|p| p.processors.iter().find(|p| p.type_id == "builtin.vad" && p.enabled));
    let vad_threshold_db = vad_proc
        .and_then(|p| p.settings.get("threshold_db"))
        .and_then(|v| v.as_f64())
        .map(|t| t as f32);
    // Only surface a separate release tick when hysteresis is actually
    // configured (release strictly below trigger); equal/above collapses
    // to the no-hysteresis case the processor clamps to at runtime.
    let vad_release_db = vad_proc
        .and_then(|p| p.settings.get("release_threshold_db"))
        .and_then(|v| v.as_f64())
        .map(|t| t as f32)
        .zip(vad_threshold_db)
        .filter(|(release, trigger)| release < trigger)
        .map(|(release, _)| release);

    // The denoise stage runs its own voice gate (RNNoise probability),
    // which doesn't map onto the dB meter's axis. We still want users
    // to know it's the one gating, so label it but skip the threshold
    // tick. Energy VAD wins the visualization if both are enabled.
    let rnnoise_vad_gating = vad_threshold_db.is_none()
        && vad_overlay
            .map(|p| {
                p.processors
                    .iter()
                    .any(|p| p.type_id == "builtin.denoise" && p.enabled && bool_setting(p, "vad_enabled"))
            })
            .unwrap_or(false);

    let level_label = match level_db {
        Some(db) => format!("{db:.0} dB"),
        None => "—".to_string(),
    };

    let bar = match vad_threshold_db {
        Some(t_db) => threshold_bar(
            value_n,
            vu_normalize(t_db),
            vad_release_db.map(vu_normalize),
            VU_BAR_W,
            VU_BAR_H,
        ),
        None => {
            // No threshold to break against — fall back to a plain
            // loudness meter coloured by current level.
            let color = match level_db {
                Some(db) if db > -3.0 => tokens::DESTRUCTIVE,
                Some(db) if db > -12.0 => tokens::WARNING,
                Some(_) => tokens::SUCCESS,
                None => tokens::MUTED,
            };
            progress(value_n, color)
                .width(Size::Fixed(VU_BAR_W))
                .height(Size::Fixed(VU_BAR_H))
        }
    };

    let mut children: Vec<El> = vec![
        row([bar, text(level_label).mono().font_size(tokens::TEXT_XS.size)])
            .gap(tokens::SPACE_2)
            .align(Align::Center),
    ];
    // Threshold-label sub-line: only printed when the caller opted into
    // the VAD overlay. The Devices meter is just "is your mic alive?"
    // — adding a VAD comment there would be noise.
    if vad_overlay.is_some() {
        let threshold_label = match (vad_threshold_db, vad_release_db, rnnoise_vad_gating) {
            (Some(trigger), Some(release), _) => {
                format!("VAD: trigger {trigger:.0} dB · release {release:.0} dB")
            }
            (Some(trigger), None, _) => format!("VAD threshold: {trigger:.0} dB"),
            (None, _, true) => "Voice gate active (RNNoise) — bar shows post-pipeline level".to_string(),
            (None, _, false) => "VAD disabled — bar shows post-pipeline level only".to_string(),
        };
        children.push(text(threshold_label).muted().font_size(tokens::TEXT_XS.size));
    }

    column(children).gap(tokens::SPACE_1).width(Size::Fill(1.0))
}

/// Four-segment bar mirroring `Mumble::AudioBar`: a darkened "won't
/// transmit" track from `0..threshold`, a darkened "will transmit"
/// track from `threshold..max`, plus brighter fills painted on top
/// proportional to the current level. The colour change at the
/// threshold position is the user's main visual cue when tuning the
/// VAD.
///
/// When `release_n` is `Some` (hysteresis configured, release < trigger)
/// a thin vertical tick is drawn at the release position inside the
/// red zone, marking where an already-active session would start
/// counting down its holdoff.
fn threshold_bar(value_n: f32, threshold_n: f32, release_n: Option<f32>, bar_w: f32, bar_h: f32) -> El {
    const TICK_W: f32 = 2.0;

    let value_n = value_n.clamp(0.0, 1.0);
    let threshold_n = threshold_n.clamp(0.0, 1.0);
    let release_n = release_n.map(|r| r.clamp(0.0, 1.0));

    let below_color = tokens::DESTRUCTIVE;
    let above_color = tokens::SUCCESS;
    let release_tick_color = tokens::WARNING;
    // Mumble darkens unfilled segments to ~1/3 brightness via Qt's
    // `darker(300)`; matching that with `darken(0.7)` keeps a hint of
    // the zone's hue on the unfilled portion so the threshold position
    // is visible even when the level is silent.
    let below_track = below_color.darken(0.7);
    let above_track = above_color.darken(0.7);

    let layout = move |ctx: LayoutCtx| {
        let r = ctx.container;
        let value_x = r.w * value_n;
        let threshold_x = r.w * threshold_n;
        let below_fill_w = value_x.min(threshold_x).max(0.0);
        let above_fill_w = (value_x - threshold_x).max(0.0);
        // Tick is sized 0 when no release threshold is configured so the
        // stack child collapses out of view without affecting layout.
        let (tick_x, tick_w) = match release_n {
            Some(rn) => {
                let center = r.w * rn;
                let x = (center - TICK_W * 0.5).clamp(0.0, (r.w - TICK_W).max(0.0));
                (r.x + x, TICK_W)
            }
            None => (r.x, 0.0),
        };
        vec![
            Rect::new(r.x, r.y, threshold_x, r.h),
            Rect::new(r.x + threshold_x, r.y, (r.w - threshold_x).max(0.0), r.h),
            Rect::new(r.x, r.y, below_fill_w, r.h),
            Rect::new(r.x + threshold_x, r.y, above_fill_w, r.h),
            Rect::new(tick_x, r.y, tick_w, r.h),
        ]
    };

    stack([
        El::new(Kind::Custom("vu-below-track")).fill(below_track),
        El::new(Kind::Custom("vu-above-track")).fill(above_track),
        El::new(Kind::Custom("vu-below-fill")).fill(below_color),
        El::new(Kind::Custom("vu-above-fill")).fill(above_color),
        El::new(Kind::Custom("vu-release-tick")).fill(release_tick_color),
    ])
    .layout(layout)
    .width(Size::Fixed(bar_w))
    .height(Size::Fixed(bar_h))
}

/// Map an output [`Role`] onto a theme colour. Keeping this the single
/// point of translation means output descriptors stay colour-free (and
/// lint-clean) while the UI controls the palette.
fn role_color(role: Role) -> Color {
    match role {
        Role::Neutral => tokens::MUTED,
        Role::Inactive => tokens::DESTRUCTIVE,
        Role::Active => tokens::SUCCESS,
        Role::Warning => tokens::WARNING,
        Role::Danger => tokens::DESTRUCTIVE,
    }
}

/// Render one declared stage output (a [`OutputKind::Meter`] or
/// [`OutputKind::Indicator`]) against its live `value`. Generic over every
/// processor — the only stage-specific input is the descriptor and the
/// stage's settings (used to resolve [`MarkAnchor::Setting`] marks).
fn render_output(spec: &OutputSpec, value: f32, settings: &JsonValue) -> El {
    match &spec.kind {
        OutputKind::Meter {
            min,
            max,
            unit,
            zones,
            marks,
        } => {
            let span = (max - min).max(f32::EPSILON);
            let norm = |v: f32| ((v - min) / span).clamp(0.0, 1.0);
            let value_n = norm(value);

            // Resolve each anchor against the stage's live settings, so a
            // `Setting`-anchored bound or tick reads the slider value. A
            // missing key drops that zone/mark rather than guessing.
            let lookup = |k: &str| settings.get(k).and_then(|v| v.as_f64()).map(|v| v as f32);

            // Zones resolved to axis units once: setting-anchored edges
            // (e.g. the VAD bands tracking trigger/release) move with the
            // sliders. Reused for the bands, the value-fill colour, and the
            // label strip so they can't disagree.
            let zones_resolved: Vec<(f32, f32, Role, Option<String>)> = zones
                .iter()
                .filter_map(|z| {
                    let lo = z.lo.resolve(lookup)?;
                    let hi = z.hi.resolve(lookup)?;
                    Some((lo, hi, z.role, z.label.clone()))
                })
                .collect();

            // Zones → normalised bands carrying their role colour.
            let zone_bands: Vec<(f32, f32, Color)> = zones_resolved
                .iter()
                .map(|(lo, hi, role, _)| (norm(*lo), norm(*hi), role_color(*role)))
                .collect();

            // Resolve marks once: `Setting` anchors read the stage's live
            // settings so the tick tracks the slider. The raw value and
            // label are kept for the legend below; the normalised position
            // drives the on-bar tick.
            let marks_resolved: Vec<(f32, Color, Option<String>)> = marks
                .iter()
                .filter_map(|m| {
                    m.at.resolve(lookup)
                        .map(|raw| (raw, role_color(m.role), m.label.clone()))
                })
                .collect();
            let mark_ticks: Vec<(f32, Color)> = marks_resolved.iter().map(|(raw, c, _)| (norm(*raw), *c)).collect();

            let readout = match unit {
                Some(u) => format!("{value:.2} {u}"),
                None => format!("{value:.2}"),
            };

            let mut col: Vec<El> = vec![
                row([
                    output_meter_bar(value_n, zone_bands, mark_ticks, VU_BAR_W, VU_BAR_H),
                    text(readout).mono().font_size(tokens::TEXT_XS.size),
                ])
                .gap(tokens::SPACE_2)
                .align(Align::Center),
            ];

            // Spatial zone labels, centred under their bands, directly
            // beneath the bar (which sits at the column's left edge).
            if let Some(strip) = zone_label_strip(&zones_resolved, *min, *max, VU_BAR_W) {
                col.push(strip);
            }

            // Footer: the output title, then a colour-keyed legend for each
            // labelled mark with its resolved value (e.g. "trigger 0.50").
            // Marks can sit too close to label spatially, so the legend
            // carries the names while the swatch colour ties back to the
            // on-bar tick.
            let mut footer: Vec<El> = vec![text(spec.title.clone()).muted().font_size(tokens::TEXT_XS.size)];
            for (raw, color, label) in &marks_resolved {
                if let Some(label) = label {
                    footer.push(
                        text(format!("{label} {raw:.2}"))
                            .text_color(*color)
                            .font_size(tokens::TEXT_XS.size),
                    );
                }
            }
            col.push(row(footer).gap(tokens::SPACE_3).align(Align::Center));

            column(col).gap(tokens::SPACE_1).width(Size::Fill(1.0))
        }
        OutputKind::Indicator => {
            let on = value != 0.0;
            let (color, label) = if on {
                (tokens::SUCCESS, "open")
            } else {
                (tokens::MUTED, "closed")
            };
            field_row(
                spec.title.clone(),
                row([
                    El::new(Kind::Custom("output-indicator-dot"))
                        .fill(color)
                        .width(Size::Fixed(10.0))
                        .height(Size::Fixed(10.0)),
                    text(label).muted().font_size(tokens::TEXT_XS.size),
                ])
                .gap(tokens::SPACE_2)
                .align(Align::Center),
            )
        }
    }
}

/// Generic meter bar: a faint full-width track, always-visible dim zone
/// bands, a bright per-zone "lit" overlay that the live value illuminates
/// from the left, and point ticks — all positioned by a single layout
/// pass. The value *lights up* the static zones rather than painting over
/// them: each band shows in its own role colour at full brightness up to
/// the current level and dimmed beyond it, so the coloured regions and
/// their labels stay legible while the speaker talks.
fn output_meter_bar(
    value_n: f32,
    zones: Vec<(f32, f32, Color)>,
    marks: Vec<(f32, Color)>,
    bar_w: f32,
    bar_h: f32,
) -> El {
    const TICK_W: f32 = 2.0;
    let value_n = value_n.clamp(0.0, 1.0);

    // Positions captured for the layout closure (colours live in the El
    // children below, in matching order). For each zone we keep both its
    // full extent (the dim base) and the portion below the current value
    // (the lit overlay), so the level reads as brightness within a band.
    let zone_pos: Vec<(f32, f32)> = zones
        .iter()
        .map(|(lo, hi, _)| (lo.clamp(0.0, 1.0), hi.clamp(0.0, 1.0)))
        .collect();
    let mark_pos: Vec<f32> = marks.iter().map(|(p, _)| p.clamp(0.0, 1.0)).collect();

    // Children order MUST match the rect order produced by `layout`:
    // [track, dim-zone*, lit-zone*, mark*]. Dim bands paint first as the
    // static base; the lit bands sit on top but only brighten their own
    // colour, so nothing is hidden.
    let mut children: Vec<El> = Vec::with_capacity(1 + zones.len() * 2 + marks.len());
    children.push(El::new(Kind::Custom("om-track")).fill(tokens::MUTED.darken(0.7)));
    for (_, _, color) in &zones {
        // Dimmed base: always visible, regardless of the live level.
        children.push(El::new(Kind::Custom("om-zone")).fill(color.darken(0.55)));
    }
    for (_, _, color) in &zones {
        // Full-brightness overlay clipped to the lit portion of the band.
        children.push(El::new(Kind::Custom("om-lit")).fill(*color));
    }
    for (_, color) in &marks {
        children.push(El::new(Kind::Custom("om-mark")).fill(*color));
    }

    let layout = move |ctx: LayoutCtx| {
        let r = ctx.container;
        let mut rects: Vec<Rect> = Vec::with_capacity(1 + zone_pos.len() * 2 + mark_pos.len());
        // Full-width background track.
        rects.push(Rect::new(r.x, r.y, r.w, r.h));
        // Dim zone bands across their full extent.
        for (lo, hi) in &zone_pos {
            let x = r.x + r.w * lo;
            let w = (r.w * (hi - lo)).max(0.0);
            rects.push(Rect::new(x, r.y, w, r.h));
        }
        // Lit overlay: each band brightened from its start up to the level.
        for (lo, hi) in &zone_pos {
            let lit_hi = value_n.clamp(*lo, *hi);
            let x = r.x + r.w * lo;
            let w = (r.w * (lit_hi - lo)).max(0.0);
            rects.push(Rect::new(x, r.y, w, r.h));
        }
        // Point ticks, centred on their position and clamped in-bounds.
        for p in &mark_pos {
            let center = r.x + r.w * p;
            let x = (center - TICK_W * 0.5).clamp(r.x, r.x + (r.w - TICK_W).max(0.0));
            rects.push(Rect::new(x, r.y, TICK_W, r.h));
        }
        rects
    };

    stack(children)
        .layout(layout)
        .width(Size::Fixed(bar_w))
        .height(Size::Fixed(bar_h))
}

/// A `bar_w`-wide strip of zone labels, each centred under its band, meant
/// to sit directly beneath the meter bar. Uncovered ranges become fixed
/// spacers so labels stay aligned to their zones even with gaps. Returns
/// `None` when no zone carries a label (nothing to show).
fn zone_label_strip(zones: &[(f32, f32, Role, Option<String>)], min: f32, max: f32, bar_w: f32) -> Option<El> {
    if !zones.iter().any(|(_, _, _, label)| label.is_some()) {
        return None;
    }
    let span = (max - min).max(f32::EPSILON);
    let norm = |v: f32| ((v - min) / span).clamp(0.0, 1.0);

    // Walk zones left-to-right, emitting a spacer for any gap before each
    // band and a centred label cell for the band itself.
    let mut ordered: Vec<&(f32, f32, Role, Option<String>)> = zones.iter().collect();
    ordered.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    let mut cells: Vec<El> = Vec::new();
    let mut cursor = 0.0f32;
    for (lo, hi, _, label) in ordered {
        let lo_n = norm(*lo);
        let hi_n = norm(*hi).max(lo_n);
        if lo_n > cursor + f32::EPSILON {
            cells.push(El::new(Kind::Group).width(Size::Fixed((lo_n - cursor) * bar_w)));
        }
        let w = (hi_n - lo_n) * bar_w;
        let label = label.clone().unwrap_or_default();
        cells.push(
            row([text(label).muted().font_size(tokens::TEXT_XS.size)])
                .width(Size::Fixed(w))
                .justify(Justify::Center),
        );
        cursor = hi_n.max(cursor);
    }

    Some(row(cells).gap(0.0).width(Size::Fixed(bar_w)))
}

fn render_sounds(pending: &PendingSettings) -> El {
    let master = section_card(
        "Master",
        [
            field_row("Enable sound effects", switch(pending.sfx_enabled).key(KEY_SFX_ENABLED)),
            field_row(
                format!("Volume ({}%)", (pending.sfx_volume * 100.0).round() as i32),
                slider(pending.sfx_volume, tokens::PRIMARY)
                    .key(KEY_SFX_VOLUME)
                    .width(Size::Fixed(280.0)),
            ),
        ],
    );

    let mut individual_rows: Vec<El> = Vec::new();
    for (idx, kind) in SfxKind::all().iter().enumerate() {
        let enabled = pending.sfx_kind_enabled.get(idx).copied().unwrap_or(true);
        individual_rows.push(
            row([
                text(kind.label().to_string()).label(),
                spacer(),
                button("Preview").key(sfx_preview_key(idx)).secondary(),
                switch(enabled).key(sfx_kind_key(idx)),
            ])
            .gap(tokens::SPACE_2)
            .align(Align::Center)
            .width(Size::Fill(1.0)),
        );
    }

    column([master, section_card("Individual sounds", individual_rows)])
        .gap(tokens::SPACE_3)
        .width(Size::Fill(1.0))
}

fn render_chat(pending: &PendingSettings) -> El {
    column([
        section_card(
            "Timestamps",
            [
                field_row(
                    "Show timestamps",
                    switch(pending.show_timestamps).key(KEY_CHAT_SHOW_TIMESTAMPS),
                ),
                field_row(
                    "Timestamp format",
                    select_trigger(KEY_CHAT_FORMAT, pending.timestamp_format.label()).width(Size::Fixed(280.0)),
                ),
            ],
        ),
        section_card(
            "History",
            [
                field_row(
                    "Auto-sync history on join",
                    switch(pending.auto_sync_history).key(KEY_CHAT_AUTO_SYNC),
                ),
                paragraph(
                    "Asks peers for backlog when joining a room so you can read what was said before you arrived.",
                )
                .muted()
                .font_size(tokens::TEXT_XS.size),
            ],
        ),
        section_card(
            "Media",
            [
                field_row(
                    "Auto-play animated images",
                    switch(pending.gif_autoplay).key(KEY_CHAT_GIF_AUTOPLAY),
                ),
                paragraph("Off shows the first frame with a play button overlay until you start it.")
                    .muted()
                    .font_size(tokens::TEXT_XS.size),
            ],
        ),
    ])
    .gap(tokens::SPACE_3)
    .width(Size::Fill(1.0))
}

/// Mumble-style Shortcuts table. Header checkbox + columns
/// Function | Data | Shortcut. Function and Data are damascene select
/// dropdowns; the Shortcut cell is a button that either enters
/// capture mode (X11 / Windows / macOS) or opens the compositor's
/// system shortcut dialog (Wayland portal).
fn render_shortcuts(pending: &PendingSettings, state: &SettingsState, hotkeys: &HotkeyManager) -> El {
    let mut shortcut_rows: Vec<El> = Vec::new();

    shortcut_rows.push(field_row(
        "Enable global shortcuts",
        switch(pending.keyboard.global_hotkeys_enabled).key(KEY_SHORTCUTS_GLOBAL_ENABLE),
    ));

    let is_wayland = hotkeys.is_wayland();
    let helper = if is_wayland {
        "Wayland binds keys through the system shortcut settings — click any Shortcut cell to open it."
    } else {
        "Click a Shortcut cell to bind a key combination; press Escape to cancel capture."
    };
    shortcut_rows.push(paragraph(helper).muted().font_size(tokens::TEXT_XS.size));

    // Column header. Width budgets match the row body below so the
    // labels line up against the dropdown triggers / shortcut buttons.
    shortcut_rows.push(
        row([
            text("Function")
                .muted()
                .font_size(tokens::TEXT_XS.size)
                .width(Size::Fixed(SHORTCUT_FN_COL_W)),
            text("Data")
                .muted()
                .font_size(tokens::TEXT_XS.size)
                .width(Size::Fixed(SHORTCUT_DATA_COL_W)),
            text("Shortcut")
                .muted()
                .font_size(tokens::TEXT_XS.size)
                .width(Size::Fill(1.0)),
            spacer().width(Size::Fixed(28.0)),
        ])
        .gap(tokens::SPACE_2)
        .align(Align::Center)
        .width(Size::Fill(1.0)),
    );

    if pending.keyboard.shortcuts.is_empty() {
        shortcut_rows.push(
            paragraph("No shortcuts configured — click Add to create one.")
                .muted()
                .font_size(tokens::TEXT_XS.size),
        );
    } else {
        for entry in &pending.keyboard.shortcuts {
            shortcut_rows.push(shortcut_row(entry, state, hotkeys, is_wayland));
        }
    }

    shortcut_rows.push(
        row([
            button_with_icon(IconName::Plus, "Add")
                .key(KEY_SHORTCUTS_ADD)
                .secondary(),
            spacer(),
        ])
        .gap(tokens::SPACE_2)
        .width(Size::Fill(1.0)),
    );

    let mut cards: Vec<El> = vec![section_card("Shortcuts", shortcut_rows)];

    if is_wayland {
        cards.push(section_card(
            "Use your desktop's shortcut settings (advanced)",
            [
                paragraph(
                    "If the portal flow above doesn't suit your compositor (tiling WMs like Sway / Hyprland, or \
                     GNOME's Custom Shortcuts), bind any key in your desktop's keyboard settings to a shell command \
                     that talks to rumble over a Unix socket. Note: most desktop shortcut systems only fire on key \
                     press, so this path supports toggle / mute / deafen well but isn't ideal for hold-style \
                     push-to-talk.",
                )
                .muted()
                .font_size(tokens::TEXT_XS.size),
                paragraph(
                    "Start damascene with `--rpc-server` to enable the socket at \
                     $XDG_RUNTIME_DIR/rumble/damascene.sock.",
                )
                .muted()
                .font_size(tokens::TEXT_XS.size),
                rpc_example_row("Toggle mute", RPC_EXAMPLE_TOGGLE_MUTE),
                rpc_example_row("Toggle deafen", RPC_EXAMPLE_TOGGLE_DEAFEN),
                rpc_example_row("Start transmit (PTT)", RPC_EXAMPLE_START_TRANSMIT),
                rpc_example_row("Stop transmit (PTT)", RPC_EXAMPLE_STOP_TRANSMIT),
            ],
        ));
    }

    column(cards).gap(tokens::SPACE_3).width(Size::Fill(1.0))
}

/// Example shell commands the user can paste into their DE's custom-
/// shortcut configuration. `nc -U` is widely available and matches the
/// RPC server's newline-delimited JSON protocol.
const RPC_EXAMPLE_TOGGLE_MUTE: &str =
    r#"echo '{"method":"set_muted","muted":true}' | nc -UN "$XDG_RUNTIME_DIR/rumble/damascene.sock""#;
const RPC_EXAMPLE_TOGGLE_DEAFEN: &str =
    r#"echo '{"method":"set_deafened","deafened":true}' | nc -UN "$XDG_RUNTIME_DIR/rumble/damascene.sock""#;
const RPC_EXAMPLE_START_TRANSMIT: &str =
    r#"echo '{"method":"start_transmit"}' | nc -UN "$XDG_RUNTIME_DIR/rumble/damascene.sock""#;
const RPC_EXAMPLE_STOP_TRANSMIT: &str =
    r#"echo '{"method":"stop_transmit"}' | nc -UN "$XDG_RUNTIME_DIR/rumble/damascene.sock""#;

/// One example row in the RPC hint: a label on the left, the command
/// text in monospace on the right (ellipsised; the user can read it in
/// the labels.txt dump if it wraps off-screen).
fn rpc_example_row(label: &str, command: &str) -> El {
    row([
        text(label)
            .muted()
            .font_size(tokens::TEXT_XS.size)
            .width(Size::Fixed(160.0)),
        mono(command)
            .font_size(tokens::TEXT_XS.size)
            .ellipsis()
            .width(Size::Fill(1.0)),
    ])
    .gap(tokens::SPACE_2)
    .align(Align::Center)
    .width(Size::Fill(1.0))
}

/// Layout constants for the shortcuts table columns. Tuned so labels
/// like "Push-to-Talk" and the longer data variants fit without
/// ellipsis.
const SHORTCUT_FN_COL_W: f32 = 160.0;
const SHORTCUT_DATA_COL_W: f32 = 110.0;

/// One row of the Shortcuts table. The trailing icon-only button
/// removes this specific row — there's no separate row-selection step,
/// which sidesteps the "selecting requires clicking the binding cell,
/// but that also opens the portal" conflict on Wayland.
fn shortcut_row(entry: &ShortcutEntry, state: &SettingsState, hotkeys: &HotkeyManager, is_wayland: bool) -> El {
    let function_trigger =
        select_trigger(shortcut_function_key(&entry.id), entry.function.label()).width(Size::Fixed(SHORTCUT_FN_COL_W));
    let data_trigger =
        select_trigger(shortcut_data_key(&entry.id), entry.data.label()).width(Size::Fixed(SHORTCUT_DATA_COL_W));

    let capturing = state.shortcut_capture.as_deref() == Some(entry.id.as_str());
    let binding_label = if capturing {
        "Press a key…".to_string()
    } else if is_wayland {
        hotkeys
            .portal_trigger(&entry.id)
            .unwrap_or_else(|| "Click to configure…".to_string())
    } else {
        entry
            .binding
            .as_ref()
            .map(|b| b.display())
            .unwrap_or_else(|| "Click to bind".to_string())
    };
    let mut binding_btn = button(binding_label).key(shortcut_binding_key(&entry.id)).secondary();
    if capturing {
        binding_btn = binding_btn.primary();
    }
    let binding_cell = binding_btn.width(Size::Fill(1.0));

    let remove = icon_button(IconName::X)
        .key(shortcut_remove_key(&entry.id))
        .ghost()
        .tooltip("Remove shortcut");

    row([function_trigger, data_trigger, binding_cell, remove])
        .gap(tokens::SPACE_2)
        .align(Align::Center)
        .width(Size::Fill(1.0))
        .key(shortcuts_row_key(&entry.id))
}

fn render_files(pending: &PendingSettings, selection: &Selection) -> El {
    let mut auto_dl: Vec<El> = vec![
        field_row(
            "Enable auto-download",
            switch(pending.auto_download_enabled).key(KEY_FILES_AUTO_DOWNLOAD),
        ),
        paragraph(
            "Auto-download offers whose MIME type matches a rule below, up to the per-rule size limit. Set a size of \
             `0` to keep a pattern around without it firing.",
        )
        .muted()
        .font_size(tokens::TEXT_XS.size),
        // Header row aligns with the inputs underneath. Width budgets:
        // mime grows to fill, size column is fixed (~80 px), trailing
        // remove icon is square.
        row([
            text("MIME pattern").muted().font_size(tokens::TEXT_XS.size),
            spacer(),
            text("Max size (MB)")
                .muted()
                .font_size(tokens::TEXT_XS.size)
                .width(Size::Fixed(110.0)),
            spacer().width(Size::Fixed(28.0)),
        ])
        .gap(tokens::SPACE_2)
        .align(Align::Center)
        .width(Size::Fill(1.0)),
    ];

    if pending.auto_download_rules.is_empty() {
        auto_dl.push(
            paragraph("No rules — add one below to start auto-downloading matching files.")
                .muted()
                .font_size(tokens::TEXT_XS.size),
        );
    } else {
        for (idx, rule) in pending.auto_download_rules.iter().enumerate() {
            auto_dl.push(rule_row(idx, rule, selection, pending.auto_download_enabled));
        }
    }

    let add_rule = button_with_icon(IconName::Plus, "Add rule")
        .key(KEY_FILES_RULE_ADD)
        .secondary();
    let mut action_row: Vec<El> = vec![add_rule];
    for (idx, (label, _, _)) in RULE_PRESETS.iter().enumerate() {
        action_row.push(button(format!("+ {label}")).key(preset_key(idx)).ghost());
    }
    action_row.push(spacer());
    auto_dl.push(row(action_row).gap(tokens::SPACE_2).width(Size::Fill(1.0)));

    let bandwidth = section_card(
        "Bandwidth limits",
        [
            field_row(
                format!("Download limit ({})", speed_label(pending.download_speed_kbps)),
                slider(
                    pending.download_speed_kbps as f32 / MAX_SPEED_KBPS as f32,
                    tokens::PRIMARY,
                )
                .key(KEY_FILES_DL_LIMIT)
                .width(Size::Fixed(280.0)),
            ),
            field_row(
                format!("Upload limit ({})", speed_label(pending.upload_speed_kbps)),
                slider(
                    pending.upload_speed_kbps as f32 / MAX_SPEED_KBPS as f32,
                    tokens::PRIMARY,
                )
                .key(KEY_FILES_UL_LIMIT)
                .width(Size::Fixed(280.0)),
            ),
        ],
    );

    let location = section_card(
        "Download location",
        [
            download_dir_row(pending),
            paragraph(
                "Where downloaded files are saved. Changes apply on the next connect — active transfers keep using \
                 the directory that was set when they started.",
            )
            .muted()
            .font_size(tokens::TEXT_XS.size),
        ],
    );

    column([section_card("Auto-download", auto_dl), bandwidth, location])
        .gap(tokens::SPACE_3)
        .width(Size::Fill(1.0))
}

/// Render the download-location row: path display (or a placeholder
/// for the platform default) plus the action buttons. The path text is
/// allowed to wrap so long paths don't push the buttons offscreen.
fn download_dir_row(pending: &PendingSettings) -> El {
    let (label, is_custom) = match pending.download_dir.as_ref() {
        Some(p) => (p.display().to_string(), true),
        None => ("System default (temp folder)".to_string(), false),
    };
    let path_label = mono(label)
        .font_size(tokens::TEXT_XS.size)
        .ellipsis()
        .width(Size::Fill(1.0));
    let path_label = if is_custom { path_label } else { path_label.muted() };

    let mut buttons: Vec<El> = vec![
        button("Browse…").key(KEY_FILES_DIR_BROWSE).secondary(),
        button("Open").key(KEY_FILES_DIR_OPEN).secondary(),
    ];
    if is_custom {
        buttons.push(button("Use default").key(KEY_FILES_DIR_RESET).ghost());
    }

    column([
        row([path_label]).width(Size::Fill(1.0)),
        row(buttons).gap(tokens::SPACE_2).align(Align::Center),
    ])
    .gap(tokens::SPACE_1)
    .width(Size::Fill(1.0))
}

/// One editable rule row: MIME pattern (fill width), size in MB
/// (fixed-width input), remove button. The whole row dims when
/// auto-download is off so the user gets a clear visual cue that the
/// rule list is currently inert.
fn rule_row(idx: usize, rule: &PendingAutoDownloadRule, selection: &Selection, enabled: bool) -> El {
    let mime_input = text_input_with(
        &rule.mime_pattern,
        selection,
        &rule_mime_key(idx),
        TextInputOpts::default().placeholder("e.g. image/*"),
    )
    .width(Size::Fill(1.0));
    let size_input = text_input_with(
        &rule.size_mb_text,
        selection,
        &rule_size_key(idx),
        TextInputOpts::default().placeholder("0"),
    )
    .width(Size::Fixed(110.0));
    let remove = icon_button(IconName::X)
        .key(rule_remove_key(idx))
        .ghost()
        .tooltip("Remove rule");

    let mut r = row([mime_input, size_input, remove])
        .gap(tokens::SPACE_2)
        .align(Align::Center)
        .width(Size::Fill(1.0));
    if !enabled {
        r = r.disabled();
    }
    r
}

fn render_stats(stats: &AudioStats) -> El {
    let loss_pct = stats.packet_loss_percent();
    let loss_color = if loss_pct > 5.0 {
        tokens::DESTRUCTIVE
    } else if loss_pct > 1.0 {
        tokens::WARNING
    } else {
        tokens::SUCCESS
    };

    let encoder = section_card(
        "Encoder",
        [
            stat_row(
                "Actual bitrate",
                format!("{:.1} kbps", stats.actual_bitrate_bps / 1000.0),
                None,
            ),
            stat_row(
                "Avg frame size",
                format!("{:.1} bytes", stats.avg_frame_size_bytes),
                None,
            ),
        ],
    );

    let network = section_card(
        "Network",
        [
            stat_row("Packets sent", stats.packets_sent.to_string(), None),
            stat_row("Packets received", stats.packets_received.to_string(), None),
            stat_row(
                "Packet loss",
                format!("{:.1}% ({} lost)", loss_pct, stats.packets_lost),
                Some(loss_color),
            ),
            stat_row("FEC recovered", stats.packets_recovered_fec.to_string(), None),
        ],
    );

    // PCM playback cushion in ms (48 samples = 1 ms at 48 kHz mono). One frame
    // is 20 ms; a healthy cushion is a few frames. Underruns/overflows are
    // clicks, so any nonzero count is flagged.
    let cushion_ms = stats.playback_buffer_samples as f32 / 48.0;
    let cushion_color = if cushion_ms < 20.0 {
        tokens::DESTRUCTIVE
    } else if cushion_ms < 40.0 {
        tokens::WARNING
    } else {
        tokens::SUCCESS
    };
    let flag_color = |n: u64| if n > 0 { Some(tokens::DESTRUCTIVE) } else { None };

    let playback = section_card(
        "Playback",
        [
            stat_row("Frames concealed", stats.frames_concealed.to_string(), None),
            stat_row(
                "Jitter buffer",
                format!("{} packets", stats.playback_buffer_packets),
                None,
            ),
            stat_row(
                "PCM cushion",
                format!("{cushion_ms:.0} ms ({} samples)", stats.playback_buffer_samples),
                Some(cushion_color),
            ),
            stat_row(
                "Underruns",
                stats.buffer_underruns.to_string(),
                flag_color(stats.buffer_underruns),
            ),
            stat_row(
                "Overflows",
                stats.buffer_overflows.to_string(),
                flag_color(stats.buffer_overflows),
            ),
            row([spacer(), button("Reset statistics").key(KEY_STATS_RESET).secondary()]).width(Size::Fill(1.0)),
        ],
    );

    column([encoder, network, playback])
        .gap(tokens::SPACE_3)
        .width(Size::Fill(1.0))
}

/// Git-derived build metadata shown on the About tab. Real builds read the
/// compile-time values `build.rs` bakes in; the golden harness pins a fixed
/// fixture via [`set_git_build_info_override`] so the commit hash and
/// `git describe` don't drift on every commit. Only the string contents
/// differ between the two — the About layout (ellipsis, fill width, the
/// dirty/clean color tokens) renders identically, so lint coverage of the
/// scene is unchanged.
#[derive(Clone)]
pub struct GitBuildInfo {
    pub describe: String,
    pub short_hash: String,
    pub full_hash: String,
    /// Empty when HEAD isn't tagged.
    pub tag: String,
    pub dirty: bool,
}

impl GitBuildInfo {
    /// The genuine compile-time values populated by `build.rs`.
    fn from_env() -> Self {
        GitBuildInfo {
            describe: env!("RUMBLE_DAMASCENE_GIT_DESCRIBE").to_string(),
            short_hash: env!("RUMBLE_DAMASCENE_GIT_SHORT_HASH").to_string(),
            full_hash: env!("RUMBLE_DAMASCENE_GIT_HASH").to_string(),
            tag: env!("RUMBLE_DAMASCENE_GIT_TAG").to_string(),
            dirty: env!("RUMBLE_DAMASCENE_GIT_DIRTY") == "true",
        }
    }
}

static GIT_BUILD_INFO_OVERRIDE: std::sync::OnceLock<GitBuildInfo> = std::sync::OnceLock::new();

/// Pin the About-tab git metadata to a fixed fixture. Intended for the
/// `dump_bundles` golden harness only — the real app never calls this and so
/// always renders the true `build.rs` values. First call wins; later calls
/// are ignored.
pub fn set_git_build_info_override(info: GitBuildInfo) {
    let _ = GIT_BUILD_INFO_OVERRIDE.set(info);
}

fn git_build_info() -> GitBuildInfo {
    GIT_BUILD_INFO_OVERRIDE
        .get()
        .cloned()
        .unwrap_or_else(GitBuildInfo::from_env)
}

/// Build / version info. `pkg_version` is the (deterministic) crate version;
/// the git fields come from [`git_build_info`] so the golden harness can pin
/// them. Tag is empty when HEAD isn't tagged; dirty is "true" / "false".
fn render_about() -> El {
    let pkg_version = env!("CARGO_PKG_VERSION");
    let info = git_build_info();
    let describe = info.describe.as_str();
    let short_hash = info.short_hash.as_str();
    let full_hash = info.full_hash.as_str();
    let tag = info.tag.as_str();
    let dirty = info.dirty;

    let mut rows: Vec<El> = Vec::new();
    rows.push(field_row("Package version", text(pkg_version).semibold()));

    if !tag.is_empty() {
        rows.push(field_row("Tag", text(tag).semibold()));
    }
    rows.push(field_row(
        "Commit",
        mono(format!("{short_hash} ({full_hash})"))
            .font_size(tokens::TEXT_XS.size)
            .ellipsis()
            .width(Size::Fill(1.0)),
    ));
    rows.push(field_row(
        "Working tree",
        if dirty {
            text("dirty — built from uncommitted changes").text_color(tokens::WARNING)
        } else {
            text("clean").text_color(tokens::SUCCESS)
        },
    ));
    rows.push(field_row(
        "git describe",
        mono(describe)
            .font_size(tokens::TEXT_XS.size)
            .ellipsis()
            .width(Size::Fill(1.0)),
    ));

    column([section_card("Rumble", rows)])
        .gap(tokens::SPACE_3)
        .width(Size::Fill(1.0))
}

/// Convert an damascene `KeyPress` into a `HotkeyBinding` using the same
/// key string format the `global-hotkey` crate consumes
/// (`HotkeyManager::key_string_to_code`). Returns `None` for keystrokes
/// that consist solely of modifier keys (Ctrl, Shift, etc.) or for
/// keys we don't have a stable name for — those can't be bound as a
/// global shortcut and capture mode stays open waiting for a real key.
fn keypress_to_binding(press: &KeyPress) -> Option<HotkeyBinding> {
    let key = match &press.key {
        UiKey::Enter => "Enter".to_string(),
        UiKey::Tab => "Tab".to_string(),
        UiKey::Space => "Space".to_string(),
        UiKey::ArrowUp => "ArrowUp".to_string(),
        UiKey::ArrowDown => "ArrowDown".to_string(),
        UiKey::ArrowLeft => "ArrowLeft".to_string(),
        UiKey::ArrowRight => "ArrowRight".to_string(),
        UiKey::Backspace => "Backspace".to_string(),
        UiKey::Delete => "Delete".to_string(),
        UiKey::Home => "Home".to_string(),
        UiKey::End => "End".to_string(),
        UiKey::PageUp => "PageUp".to_string(),
        UiKey::PageDown => "PageDown".to_string(),
        UiKey::Escape => return None, // Escape is the cancel shortcut.
        UiKey::Character(s) => {
            // Strip stray whitespace and normalise to uppercase so
            // "a"/"A" both round-trip through key_string_to_code.
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return None;
            }
            trimmed.to_string()
        }
        UiKey::Other(name) => {
            // F-keys, media keys, etc. show up here as winit-style
            // names. Pass through verbatim — global-hotkey's
            // key_string_to_code matches case-insensitively for F1..F12
            // and we don't try to handle the long tail of media keys.
            if name.is_empty() {
                return None;
            }
            name.clone()
        }
    };
    let modifiers = HotkeyModifiers {
        ctrl: press.modifiers.ctrl,
        shift: press.modifiers.shift,
        alt: press.modifiers.alt,
        super_key: press.modifiers.logo,
    };
    Some(HotkeyBinding { modifiers, key })
}

// ---- view helpers ---------------------------------------------------

/// Wrap a related group of settings rows in a titled card. The card
/// is the "panel" we group settings into so each tab reads as a stack
/// of bordered groups instead of a flat run of dividers + headings.
fn section_card<I, E>(title: impl Into<String>, children: I) -> El
where
    I: IntoIterator<Item = E>,
    E: Into<El>,
{
    card([
        card_header([card_title(title)]).padding(Sides {
            left: tokens::SPACE_4,
            right: tokens::SPACE_4,
            top: tokens::SPACE_3,
            bottom: tokens::SPACE_2,
        }),
        card_content(children.into_iter().map(Into::into).collect::<Vec<_>>())
            .padding(Sides {
                left: tokens::SPACE_4,
                right: tokens::SPACE_4,
                top: 0.0,
                bottom: tokens::SPACE_4,
            })
            .gap(tokens::SPACE_2),
    ])
}

fn stat_row(label: impl Into<String>, value: impl Into<String>, color: Option<Color>) -> El {
    let value_text = text(value).mono().font_size(tokens::TEXT_XS.size);
    let value_text = if let Some(c) = color {
        value_text.text_color(c)
    } else {
        value_text
    };
    row([text(label).muted(), spacer(), value_text])
        .gap(tokens::SPACE_3)
        .align(Align::Center)
        .width(Size::Fill(1.0))
}

fn device_label_for(selected: Option<&str>, devices: &[rumble_client::AudioDeviceInfo]) -> String {
    match selected {
        None => "System default".to_string(),
        Some(id) => devices
            .iter()
            .find(|d| d.id == id)
            .map(|d| d.name.clone())
            .unwrap_or_else(|| format!("(missing) {id}")),
    }
}

fn speed_label(kbps: u32) -> String {
    if kbps == 0 {
        "unlimited".to_string()
    } else {
        format!("{kbps} KB/s")
    }
}

fn bitrate_slug(bitrate: i32) -> &'static str {
    match bitrate {
        AudioSettings::BITRATE_LOW => "low",
        AudioSettings::BITRATE_MEDIUM => "medium",
        AudioSettings::BITRATE_HIGH => "high",
        AudioSettings::BITRATE_VERY_HIGH => "very-high",
        _ => "high",
    }
}

fn parse_bitrate(slug: &str) -> Option<i32> {
    Some(match slug {
        "low" => AudioSettings::BITRATE_LOW,
        "medium" => AudioSettings::BITRATE_MEDIUM,
        "high" => AudioSettings::BITRATE_HIGH,
        "very-high" => AudioSettings::BITRATE_VERY_HIGH,
        _ => return None,
    })
}

// ============================================================
// Event handling
// ============================================================

pub fn handle_event(
    state: &mut SettingsState,
    event: &UiEvent,
    app_state: &State,
    identity: &Identity,
    selection: &mut Selection,
    processor_registry: &ProcessorRegistry,
    hotkeys: &HotkeyManager,
) -> SettingsOutcome {
    if !state.open {
        return SettingsOutcome::Ignored;
    }

    // Admin tab routes first when active so its popover/dialog events
    // win over the generic dispatch below. Settings stays open after
    // admin commands fire — they're fire-and-forget against the
    // connected server.
    if matches!(state.tab, Some(SettingsTab::Admin)) && admin_visible(app_state) {
        match admin::handle_event(&mut state.admin, event, app_state, selection) {
            AdminOutcome::Ignored => {}
            AdminOutcome::Handled => return SettingsOutcome::Handled,
            AdminOutcome::Dispatch(cmds) => return SettingsOutcome::Dispatch(cmds),
        }
    }

    // Esc unconditionally cancels.
    if matches!(event.kind, UiEventKind::Escape) {
        // Capture takes priority — Escape cancels capture before
        // backing out of dropdowns or closing the dialog.
        if state.shortcut_capture.is_some() {
            state.shortcut_capture = None;
            return SettingsOutcome::Handled;
        }
        if state.open_select != OpenSelect::None {
            state.open_select = OpenSelect::None;
            return SettingsOutcome::Handled;
        }
        return SettingsOutcome::Close;
    }

    // Key capture for the Shortcuts table — only X11/Win/macOS can grab
    // keys this way; on Wayland capture is never entered.
    if state.shortcut_capture.is_some()
        && matches!(event.kind, UiEventKind::KeyDown)
        && let Some(press) = event.key_press.as_ref()
        && let Some(binding) = keypress_to_binding(press)
    {
        let entry_id = state.shortcut_capture.take().expect("checked is_some above");
        if let Some(pending) = state.pending.as_mut()
            && let Some(slot) = pending.keyboard.shortcuts.iter_mut().find(|e| e.id == entry_id)
        {
            slot.binding = Some(binding);
            let kb = pending.keyboard.clone();
            return SettingsOutcome::RegisterHotkeys(kb);
        }
        return SettingsOutcome::Handled;
    }

    // Scrim click + Close button.
    if event.is_click_or_activate(KEY_DISMISS)
        || (event.is_route(KEY_DISMISS) && event.kind == UiEventKind::Click)
        || event.is_click_or_activate(KEY_CLOSE)
    {
        return SettingsOutcome::Close;
    }

    // Apply: hand back the pending state for the App to apply while
    // keeping the dialog open. Guard here as well as in the disabled
    // button state so keyboard activation can't apply a clean snapshot.
    if event.is_click_or_activate(KEY_APPLY) {
        if !state.has_pending_changes() {
            return SettingsOutcome::Handled;
        }
        if let Some(pending) = state.pending.clone() {
            state.mark_applied();
            return SettingsOutcome::Apply(pending);
        }
        return SettingsOutcome::Handled;
    }

    // Save: hand back the pending state for the App to apply.
    if event.is_click_or_activate(KEY_SAVE) {
        if let Some(pending) = state.pending.clone() {
            return SettingsOutcome::Save(pending);
        }
        return SettingsOutcome::Close;
    }

    // Tab switching.
    if let Some(tab) = state.tab.as_mut()
        && tabs::apply_event(tab, event, KEY_TABS, SettingsTab::from_slug)
    {
        // Switching tabs implicitly closes any open dropdown so its
        // popover doesn't outlive the surface it was anchored to.
        state.open_select = OpenSelect::None;
        return SettingsOutcome::Handled;
    }

    // Identity regenerate is a one-shot side effect — close + open wizard.
    if event.is_click_or_activate(KEY_REGENERATE) {
        return SettingsOutcome::OpenIdentityWizard;
    }
    if event.is_click_or_activate(KEY_SWITCH_AGENT) {
        return SettingsOutcome::OpenIdentityWizardAgent;
    }
    // Elevate handoff: close settings and let the App open the modal.
    if event.is_click_or_activate(KEY_ELEVATE) {
        return SettingsOutcome::OpenElevate;
    }
    if event.is_click_or_activate(KEY_COPY_PUBKEY)
        && let Some(pubkey) = identity.public_key()
    {
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, pubkey);
        return SettingsOutcome::CopyPublicKey(b64);
    }
    if event.is_click_or_activate(KEY_REFRESH_DEVICES) {
        return SettingsOutcome::RefreshDevices;
    }
    if event.is_click_or_activate(KEY_STATS_RESET) {
        return SettingsOutcome::ResetStats;
    }

    let pending = match state.pending.as_mut() {
        Some(p) => p,
        None => return SettingsOutcome::Ignored,
    };

    // ---------- per-tab pending edits ----------

    // Auto-download rules editor. Add / remove buttons fire on
    // Click/Activate; the text inputs land on `target_key`, so check
    // those first to keep the per-row update before any of the
    // free-form text-input handlers further down.
    // ---------- Shortcuts tab ----------

    // Enable / disable the whole global-hotkeys system.
    if switch::apply_event(
        &mut pending.keyboard.global_hotkeys_enabled,
        event,
        KEY_SHORTCUTS_GLOBAL_ENABLE,
    ) {
        let kb = pending.keyboard.clone();
        return SettingsOutcome::RegisterHotkeys(kb);
    }

    if event.is_click_or_activate(KEY_SHORTCUTS_ADD) {
        // Default new row: Push-to-Talk / Hold / unbound. The user
        // picks a different function from the dropdown if they want
        // something else. A fresh PTT entry is the most common case
        // (e.g. a second key on a controller).
        let new_entry = ShortcutEntry {
            id: ShortcutEntry::new_id(),
            function: HotkeyFunction::PushToTalk,
            data: HotkeyData::Hold,
            binding: None,
        };
        pending.keyboard.shortcuts.push(new_entry);
        let kb = pending.keyboard.clone();
        return SettingsOutcome::RegisterHotkeys(kb);
    }

    // Per-row events. The route prefix is `settings:shortcuts:row:<id>:<field>`;
    // matches function dropdown, data dropdown, binding cell, and bare
    // row clicks (for selection).
    if let Some(route) = event.route().filter(|r| r.starts_with(SHORTCUTS_ROW_PREFIX))
        && let Some((entry_id, field_path)) = parse_shortcut_route(route)
    {
        let entry_id = entry_id.to_string();
        match shortcut_field_kind(field_path) {
            "function" => {
                let key = shortcut_function_key(&entry_id);
                if let Some(action) = select::classify_event(event, &key) {
                    match action {
                        select::SelectAction::Toggle => {
                            state.open_select = if state.open_select == OpenSelect::ShortcutFunction(entry_id.clone()) {
                                OpenSelect::None
                            } else {
                                OpenSelect::ShortcutFunction(entry_id.clone())
                            };
                        }
                        select::SelectAction::Dismiss => state.open_select = OpenSelect::None,
                        select::SelectAction::Pick(value) => {
                            state.open_select = OpenSelect::None;
                            if let Some(function) = HotkeyFunction::from_slug(&value)
                                && let Some(slot) = pending.keyboard.shortcuts.iter_mut().find(|e| e.id == entry_id)
                            {
                                slot.function = function;
                                // Snap Data to a valid variant for the
                                // new function (e.g. switching to PTT
                                // forces Data=Hold).
                                if !function.data_options().contains(&slot.data) {
                                    slot.data = function.default_data();
                                }
                                let kb = pending.keyboard.clone();
                                return SettingsOutcome::RegisterHotkeys(kb);
                            }
                        }
                        _ => {}
                    }
                    return SettingsOutcome::Handled;
                }
            }
            "data" => {
                let key = shortcut_data_key(&entry_id);
                if let Some(action) = select::classify_event(event, &key) {
                    match action {
                        select::SelectAction::Toggle => {
                            state.open_select = if state.open_select == OpenSelect::ShortcutData(entry_id.clone()) {
                                OpenSelect::None
                            } else {
                                OpenSelect::ShortcutData(entry_id.clone())
                            };
                        }
                        select::SelectAction::Dismiss => state.open_select = OpenSelect::None,
                        select::SelectAction::Pick(value) => {
                            state.open_select = OpenSelect::None;
                            if let Some(data) = HotkeyData::from_slug(&value)
                                && let Some(slot) = pending.keyboard.shortcuts.iter_mut().find(|e| e.id == entry_id)
                                && slot.function.data_options().contains(&data)
                            {
                                slot.data = data;
                                let kb = pending.keyboard.clone();
                                return SettingsOutcome::RegisterHotkeys(kb);
                            }
                        }
                        _ => {}
                    }
                    return SettingsOutcome::Handled;
                }
            }
            "binding" => {
                if matches!(event.kind, UiEventKind::Click | UiEventKind::Activate) {
                    if hotkeys.is_wayland() {
                        // Portal owns key assignment — defer to the
                        // compositor's settings dialog. We've already
                        // re-bound the shortcut by calling
                        // RegisterHotkeys on the prior edit, so the
                        // entry's id is known to the portal.
                        return SettingsOutcome::OpenPortalShortcutSettings;
                    }
                    state.shortcut_capture = Some(entry_id);
                    return SettingsOutcome::Handled;
                }
            }
            "remove" => {
                if matches!(event.kind, UiEventKind::Click | UiEventKind::Activate) {
                    pending.keyboard.shortcuts.retain(|e| e.id != entry_id);
                    if state.shortcut_capture.as_deref() == Some(entry_id.as_str()) {
                        state.shortcut_capture = None;
                    }
                    let kb = pending.keyboard.clone();
                    return SettingsOutcome::RegisterHotkeys(kb);
                }
            }
            _ => {}
        }
    }

    // ---------- Files: auto-download rules ----------

    if event.is_click_or_activate(KEY_FILES_RULE_ADD) {
        // 10 MB matches the egui client's "Add Rule" default. New
        // rows start with an empty pattern so the user types theirs
        // before the rule actually fires.
        pending.auto_download_rules.push(PendingAutoDownloadRule {
            mime_pattern: String::new(),
            size_mb_text: DEFAULT_RULE_SIZE_MB.min(MAX_RULE_SIZE_MB).to_string(),
        });
        return SettingsOutcome::Handled;
    }
    if let Some(route) = event.route()
        && matches!(event.kind, UiEventKind::Click | UiEventKind::Activate)
        && let Some(idx) = parse_preset_route(route)
        && let Some((_, pattern, size_mb)) = RULE_PRESETS.get(idx)
    {
        pending.auto_download_rules.push(PendingAutoDownloadRule {
            mime_pattern: (*pattern).to_string(),
            size_mb_text: size_mb.to_string(),
        });
        return SettingsOutcome::Handled;
    }
    if let Some(target) = event.target_key()
        && let Some((idx, field)) = parse_rule_route(target)
        && let Some(rule) = pending.auto_download_rules.get_mut(idx)
    {
        match field {
            "mime" => {
                text_input::apply_event(&mut rule.mime_pattern, selection, &rule_mime_key(idx), event);
                return SettingsOutcome::Handled;
            }
            "size" => {
                let key = rule_size_key(idx);
                text_input::apply_event(&mut rule.size_mb_text, selection, &key, event);
                return SettingsOutcome::Handled;
            }
            _ => {}
        }
    }
    if let Some(route) = event.route()
        && matches!(event.kind, UiEventKind::Click | UiEventKind::Activate)
        && let Some((idx, "remove")) = parse_rule_route(route)
        && idx < pending.auto_download_rules.len()
    {
        pending.auto_download_rules.remove(idx);
        return SettingsOutcome::Handled;
    }

    // Switches.
    if switch::apply_event(&mut pending.autoconnect, event, KEY_AUTOCONNECT)
        || switch::apply_event(&mut pending.audio.fec_enabled, event, KEY_VOICE_FEC)
        || switch::apply_event(&mut pending.sfx_enabled, event, KEY_SFX_ENABLED)
        || switch::apply_event(&mut pending.show_timestamps, event, KEY_CHAT_SHOW_TIMESTAMPS)
        || switch::apply_event(&mut pending.auto_sync_history, event, KEY_CHAT_AUTO_SYNC)
        || switch::apply_event(&mut pending.gif_autoplay, event, KEY_CHAT_GIF_AUTOPLAY)
        || switch::apply_event(&mut pending.auto_download_enabled, event, KEY_FILES_AUTO_DOWNLOAD)
    {
        return SettingsOutcome::Handled;
    }

    // Download-location buttons. Browse fires the OS picker (handled
    // by the App), Open reveals the current effective dir, Reset
    // clears the override so we fall back to the platform default.
    if event.is_click_or_activate(KEY_FILES_DIR_BROWSE) {
        return SettingsOutcome::PickDownloadDir;
    }
    if event.is_click_or_activate(KEY_FILES_DIR_OPEN) {
        return SettingsOutcome::OpenDownloadDir(effective_download_dir(pending));
    }
    if event.is_click_or_activate(KEY_FILES_DIR_RESET) {
        pending.download_dir = None;
        return SettingsOutcome::Handled;
    }

    // Per-sfx switch + preview button.
    if matches!(event.kind, UiEventKind::Click | UiEventKind::Activate)
        && let Some(route) = event.route()
    {
        if let Some(idx) = route
            .strip_prefix("settings:sfx:kind:")
            .and_then(|s| s.parse::<usize>().ok())
            && let Some(slot) = pending.sfx_kind_enabled.get_mut(idx)
        {
            *slot = !*slot;
            return SettingsOutcome::Handled;
        }
        if let Some(idx) = route
            .strip_prefix("settings:sfx:preview:")
            .and_then(|s| s.parse::<usize>().ok())
            && let Some(kind) = SfxKind::all().get(idx).copied()
        {
            return SettingsOutcome::PreviewSfx {
                kind,
                volume: pending.sfx_volume.max(0.3),
            };
        }
    }

    // Voice mode tabs.
    if tabs::apply_event(&mut pending.voice_mode, event, KEY_VOICE_MODE_TABS, |s| match s {
        "ptt" => Some(PersistentVoiceMode::PushToTalk),
        "cont" => Some(PersistentVoiceMode::Continuous),
        _ => None,
    }) {
        return SettingsOutcome::Handled;
    }

    // Bitrate tabs.
    if tabs::apply_event(&mut pending.audio.bitrate, event, KEY_BITRATE_TABS, parse_bitrate) {
        return SettingsOutcome::Handled;
    }

    // Sliders — pointer drag.
    if matches!(
        event.kind,
        UiEventKind::PointerDown | UiEventKind::Drag | UiEventKind::Click
    ) && let Some(route) = event.route()
        && let (Some(rect), Some(x)) = (event.target_rect(), event.pointer_x())
    {
        match route {
            KEY_VOICE_COMPLEXITY => {
                let n = slider::normalized_from_event(rect, x);
                pending.audio.encoder_complexity = (n * 10.0).round() as i32;
                return SettingsOutcome::Handled;
            }
            KEY_VOICE_JITTER => {
                let n = slider::normalized_from_event(rect, x);
                pending.audio.jitter_buffer_delay_packets = (1.0 + n * 9.0).round() as u32;
                return SettingsOutcome::Handled;
            }
            KEY_VOICE_PACKET_LOSS => {
                let n = slider::normalized_from_event(rect, x);
                pending.audio.packet_loss_percent = (n * 25.0).round() as i32;
                return SettingsOutcome::Handled;
            }
            KEY_SFX_VOLUME => {
                pending.sfx_volume = slider::normalized_from_event(rect, x);
                return SettingsOutcome::Handled;
            }
            KEY_FILES_DL_LIMIT => {
                let n = slider::normalized_from_event(rect, x);
                pending.download_speed_kbps = (n * MAX_SPEED_KBPS as f32).round() as u32;
                return SettingsOutcome::Handled;
            }
            KEY_FILES_UL_LIMIT => {
                let n = slider::normalized_from_event(rect, x);
                pending.upload_speed_kbps = (n * MAX_SPEED_KBPS as f32).round() as u32;
                return SettingsOutcome::Handled;
            }
            _ => {}
        }
    }

    // Sliders — keyboard arrows. Step granularity matches the pointer
    // resolution: complexity steps by 1 (=10%), packet-loss by 4%
    // (1/25), jitter by 1 packet (~11%), volume by 5%, speed by 5%.
    if let Some(route) = event.route() {
        let normalized = match route {
            KEY_VOICE_COMPLEXITY => Some(pending.audio.encoder_complexity as f32 / 10.0),
            KEY_VOICE_JITTER => Some((pending.audio.jitter_buffer_delay_packets as f32 - 1.0) / 9.0),
            KEY_VOICE_PACKET_LOSS => Some(pending.audio.packet_loss_percent as f32 / 25.0),
            KEY_SFX_VOLUME => Some(pending.sfx_volume),
            KEY_FILES_DL_LIMIT => Some(pending.download_speed_kbps as f32 / MAX_SPEED_KBPS as f32),
            KEY_FILES_UL_LIMIT => Some(pending.upload_speed_kbps as f32 / MAX_SPEED_KBPS as f32),
            _ => None,
        };
        if let Some(mut n) = normalized
            && slider::apply_event(&mut n, event, route, 0.05, 0.25)
        {
            n = n.clamp(0.0, 1.0);
            match route {
                KEY_VOICE_COMPLEXITY => pending.audio.encoder_complexity = (n * 10.0).round() as i32,
                KEY_VOICE_JITTER => pending.audio.jitter_buffer_delay_packets = (1.0 + n * 9.0).round() as u32,
                KEY_VOICE_PACKET_LOSS => pending.audio.packet_loss_percent = (n * 25.0).round() as i32,
                KEY_SFX_VOLUME => pending.sfx_volume = n,
                KEY_FILES_DL_LIMIT => pending.download_speed_kbps = (n * MAX_SPEED_KBPS as f32).round() as u32,
                KEY_FILES_UL_LIMIT => pending.upload_speed_kbps = (n * MAX_SPEED_KBPS as f32).round() as u32,
                _ => {}
            }
            return SettingsOutcome::Handled;
        }
    }

    // Selects (dropdowns). Each select fans into Toggle/Dismiss/Pick;
    // we route them by hand because the typed value the select picks
    // varies (Option<String> for devices, TimestampFormat for chat).
    if let Some(action) = select::classify_event(event, KEY_INPUT_DEVICE) {
        match action {
            select::SelectAction::Toggle => {
                state.open_select = if state.open_select == OpenSelect::InputDevice {
                    OpenSelect::None
                } else {
                    OpenSelect::InputDevice
                };
            }
            select::SelectAction::Dismiss => state.open_select = OpenSelect::None,
            select::SelectAction::Pick(value) => {
                pending.input_device = (value != "__default").then(|| value.to_string());
                state.open_select = OpenSelect::None;
            }
            // SelectAction is #[non_exhaustive] — fall through harmlessly
            // for any new variant damascene adds upstream.
            _ => {}
        }
        return SettingsOutcome::Handled;
    }
    if let Some(action) = select::classify_event(event, KEY_OUTPUT_DEVICE) {
        match action {
            select::SelectAction::Toggle => {
                state.open_select = if state.open_select == OpenSelect::OutputDevice {
                    OpenSelect::None
                } else {
                    OpenSelect::OutputDevice
                };
            }
            select::SelectAction::Dismiss => state.open_select = OpenSelect::None,
            select::SelectAction::Pick(value) => {
                pending.output_device = (value != "__default").then(|| value.to_string());
                state.open_select = OpenSelect::None;
            }
            _ => {}
        }
        return SettingsOutcome::Handled;
    }
    if let Some(action) = select::classify_event(event, KEY_CHAT_FORMAT) {
        match action {
            select::SelectAction::Toggle => {
                state.open_select = if state.open_select == OpenSelect::TimestampFormat {
                    OpenSelect::None
                } else {
                    OpenSelect::TimestampFormat
                };
            }
            select::SelectAction::Dismiss => state.open_select = OpenSelect::None,
            select::SelectAction::Pick(value) => {
                if let Ok(idx) = value.parse::<usize>()
                    && let Some(fmt) = TimestampFormat::ALL.get(idx).copied()
                {
                    pending.timestamp_format = fmt;
                }
                state.open_select = OpenSelect::None;
            }
            _ => {}
        }
        return SettingsOutcome::Handled;
    }

    // ---------- Processing tab: dynamic per-processor controls ----------
    //
    // Routes here are built at render time as `settings:proc:<idx>:<slot>`
    // — see [`ProcRouteSlot`] for the slot vocabulary (enable switch,
    // schema field, move-up / move-down / remove buttons). We dispatch
    // text/switch/slider events against whichever shape the schema
    // picked, so the keyboard + pointer codepaths converge into a
    // single update of `pending.tx_pipeline`.

    // "Add processor" dropdown. Picking a type appends a fresh
    // `ProcessorConfig` (defaults from the registry) to the end of the
    // pipeline — the user then moves / reorders / configures from
    // there. Duplicates of the same type_id are allowed; the pipeline
    // is keyed by index, not type, and a chain like "gain → denoise →
    // gain" is a legitimate use case.
    if let Some(action) = select::classify_event(event, KEY_PROC_ADD) {
        match action {
            select::SelectAction::Toggle => {
                state.open_select = if state.open_select == OpenSelect::AddProcessor {
                    OpenSelect::None
                } else {
                    OpenSelect::AddProcessor
                };
            }
            select::SelectAction::Dismiss => state.open_select = OpenSelect::None,
            select::SelectAction::Pick(value) => {
                if let Some(mut new_config) = processor_registry.default_config(&value) {
                    // Default to enabled — that's what the user just
                    // explicitly asked for. The factory's
                    // `default_settings()` already populates each
                    // schema field, so no backfill is needed.
                    new_config.enabled = true;
                    pending.tx_pipeline.processors.push(new_config);
                }
                state.open_select = OpenSelect::None;
            }
            _ => {}
        }
        return SettingsOutcome::Handled;
    }

    // Text-input events arrive on `target_key` rather than `route`, so
    // check this before the generic `route()`-driven handlers.
    if let Some(target) = event.target_key()
        && let Some((idx, ProcRouteSlot::Field(field))) = parse_proc_route(target)
        && let Some(proc_config) = pending.tx_pipeline.processors.get_mut(idx)
        && schema_field_type(processor_registry, &proc_config.type_id, field) == Some("string")
    {
        let route = proc_field_key(idx, field);
        let mut value = proc_config
            .settings
            .get(field)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        text_input::apply_event(&mut value, selection, &route, event);
        ensure_object(&mut proc_config.settings)[field] = JsonValue::String(value);
        return SettingsOutcome::Handled;
    }

    if let Some(route) = event.route()
        && let Some((idx, slot)) = parse_proc_route(route)
    {
        // Mutating actions on the pipeline list itself. These can fire
        // even when `idx` would otherwise be out of bounds for a
        // settings access, so handle them before the per-processor
        // `get_mut` below — though in practice the route was just
        // produced by the render at `idx`, so it should always be live.
        if matches!(event.kind, UiEventKind::Click | UiEventKind::Activate) {
            match slot {
                ProcRouteSlot::MoveUp => {
                    if idx > 0 && idx < pending.tx_pipeline.processors.len() {
                        pending.tx_pipeline.processors.swap(idx - 1, idx);
                    }
                    return SettingsOutcome::Handled;
                }
                ProcRouteSlot::MoveDown => {
                    if idx + 1 < pending.tx_pipeline.processors.len() {
                        pending.tx_pipeline.processors.swap(idx, idx + 1);
                    }
                    return SettingsOutcome::Handled;
                }
                ProcRouteSlot::Remove => {
                    if idx < pending.tx_pipeline.processors.len() {
                        pending.tx_pipeline.processors.remove(idx);
                    }
                    return SettingsOutcome::Handled;
                }
                _ => {}
            }
        }

        let Some(proc_config) = pending.tx_pipeline.processors.get_mut(idx) else {
            return SettingsOutcome::Ignored;
        };

        match slot {
            // Per-processor enable switch.
            ProcRouteSlot::Enabled => {
                if switch::apply_event(&mut proc_config.enabled, event, route) {
                    return SettingsOutcome::Handled;
                }
            }
            // Action buttons handled above; we only get here for non-
            // Click/Activate events (e.g. focus traversal), which the
            // ghost icon-button surface absorbs on its own.
            ProcRouteSlot::MoveUp | ProcRouteSlot::MoveDown | ProcRouteSlot::Remove => {}
            // Schema-driven field. The schema lookup is the source of
            // truth for "what control type was rendered" — the event
            // codepaths can't peek at the rendered tree.
            ProcRouteSlot::Field(field) => {
                let prop_type = schema_field_type(processor_registry, &proc_config.type_id, field);
                let prop_schema = processor_registry
                    .settings_schema(&proc_config.type_id)
                    .and_then(|s| s.get("properties").cloned())
                    .and_then(|p| p.get(field).cloned());

                match prop_type {
                    Some("boolean") => {
                        // After `merge_with_default_tx_pipeline` runs,
                        // every schema-defined field is present on the
                        // persisted config. The schema-default fallback
                        // is the same one the display path uses
                        // (`render_schema_field`), so a missing field
                        // can't make the toggle's "current value" disagree
                        // with what's painted on screen.
                        let schema_default = prop_schema
                            .as_ref()
                            .and_then(|s| s.get("default"))
                            .and_then(|d| d.as_bool())
                            .unwrap_or(false);
                        let mut value = proc_config
                            .settings
                            .get(field)
                            .and_then(|v| v.as_bool())
                            .unwrap_or(schema_default);
                        if switch::apply_event(&mut value, event, route) {
                            ensure_object(&mut proc_config.settings)[field] = JsonValue::Bool(value);
                            return SettingsOutcome::Handled;
                        }
                    }
                    Some("number") => {
                        if let Some(schema) = prop_schema.as_ref() {
                            let min = schema.get("minimum").and_then(|m| m.as_f64()).unwrap_or(-100.0) as f32;
                            let max = schema.get("maximum").and_then(|m| m.as_f64()).unwrap_or(100.0) as f32;
                            let span = (max - min).max(f32::EPSILON);

                            // Pointer drag.
                            if matches!(
                                event.kind,
                                UiEventKind::PointerDown | UiEventKind::Drag | UiEventKind::Click
                            ) && let (Some(rect), Some(x)) = (event.target_rect(), event.pointer_x())
                            {
                                let n = slider::normalized_from_event(rect, x);
                                let value = (min + n * span).clamp(min, max);
                                ensure_object(&mut proc_config.settings)[field] = serde_json::json!(value as f64);
                                return SettingsOutcome::Handled;
                            }

                            // Keyboard nudge.
                            let current = proc_config
                                .settings
                                .get(field)
                                .and_then(|v| v.as_f64())
                                .map(|v| v as f32)
                                .unwrap_or(min);
                            let mut n = ((current - min) / span).clamp(0.0, 1.0);
                            if slider::apply_event(&mut n, event, route, 0.05, 0.25) {
                                let value = (min + n.clamp(0.0, 1.0) * span).clamp(min, max);
                                ensure_object(&mut proc_config.settings)[field] = serde_json::json!(value as f64);
                                return SettingsOutcome::Handled;
                            }
                        }
                    }
                    Some("integer") => {
                        if let Some(schema) = prop_schema.as_ref() {
                            let min = schema.get("minimum").and_then(|m| m.as_i64()).unwrap_or(0);
                            let max = schema.get("maximum").and_then(|m| m.as_i64()).unwrap_or(100);
                            let span = (max - min).max(1) as f32;

                            if matches!(
                                event.kind,
                                UiEventKind::PointerDown | UiEventKind::Drag | UiEventKind::Click
                            ) && let (Some(rect), Some(x)) = (event.target_rect(), event.pointer_x())
                            {
                                let n = slider::normalized_from_event(rect, x);
                                let value = (min as f32 + n * span).round().clamp(min as f32, max as f32) as i64;
                                ensure_object(&mut proc_config.settings)[field] = serde_json::json!(value);
                                return SettingsOutcome::Handled;
                            }

                            let current = proc_config.settings.get(field).and_then(|v| v.as_i64()).unwrap_or(min);
                            let mut n = ((current - min) as f32 / span).clamp(0.0, 1.0);
                            if slider::apply_event(&mut n, event, route, 0.05, 0.25) {
                                let value = (min as f32 + n.clamp(0.0, 1.0) * span)
                                    .round()
                                    .clamp(min as f32, max as f32) as i64;
                                ensure_object(&mut proc_config.settings)[field] = serde_json::json!(value);
                                return SettingsOutcome::Handled;
                            }
                        }
                    }
                    _ => {
                        // String / unknown — text-input branch handles
                        // those above via `target_key`.
                    }
                }
            }
        }
    }

    SettingsOutcome::Ignored
}

/// Look up the JSON-Schema `type` for a single property of the named
/// processor. Returns `None` when the processor or property is unknown
/// — callers fall through to "ignore the event" in that case.
fn schema_field_type(registry: &ProcessorRegistry, type_id: &str, field: &str) -> Option<&'static str> {
    let schema = registry.settings_schema(type_id)?;
    let properties = schema.get("properties")?.as_object()?;
    let prop = properties.get(field)?;
    let ty = prop.get("type")?.as_str()?;
    // `serde_json::Value` strings are `&str` borrowed from the value
    // itself, not `&'static`. The set of types we care about is small
    // and known, so map them to `'static` literals to avoid leaking
    // the borrow into the caller's lifetime.
    Some(match ty {
        "number" => "number",
        "integer" => "integer",
        "boolean" => "boolean",
        "string" => "string",
        _ => return None,
    })
}

/// `serde_json::Value::object_mut` doesn't exist on stable; this helper
/// makes sure `value` is an object so we can index into it. Replaces a
/// non-object `value` with an empty object as a side effect.
fn ensure_object(value: &mut JsonValue) -> &mut JsonValue {
    if !value.is_object() {
        *value = JsonValue::Object(serde_json::Map::new());
    }
    value
}
