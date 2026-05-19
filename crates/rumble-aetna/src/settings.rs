//! Settings dialog — tab-based configuration UI for the aetna client.
//!
//! Owns its own ephemeral UI state (selected tab, pending values for
//! each section, which dropdown is open). The App routes events here
//! via [`handle_event`] and dispatches the resulting [`SettingsOutcome`]
//! back to the backend / `SettingsStore` / identity wizard.
//!
//! Save semantics mirror the rumble-egui dialog: most edits accumulate
//! in `pending_*` fields and only land when the user clicks Save. A few
//! controls (Refresh devices, Reset stats, Preview sfx, Regenerate
//! identity) fire immediately because they're side-effecting actions
//! rather than persisted state.

use std::path::PathBuf;

use aetna_core::prelude::*;

use rumble_client::{PipelineConfig, ProcessorRegistry, SfxKind};
use rumble_desktop_shell::{
    AutoDownloadRule, HotkeyBinding, HotkeyData, HotkeyFunction, HotkeyManager, HotkeyModifiers, KeyboardSettings,
    PersistentVoiceMode, Settings, ShortcutEntry, TimestampFormat,
};
use rumble_protocol::{AudioSettings, AudioState, State, permissions::Permissions};
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
    /// Function dropdown open on the shortcut entry with this id.
    ShortcutFunction(String),
    /// Data dropdown open on the shortcut entry with this id.
    ShortcutData(String),
}

/// Pending edits accumulated while the dialog is open. Initialised
/// from the live values in [`SettingsState::open_with`] and read back
/// by [`SettingsOutcome::Save`].
#[derive(Debug, Clone)]
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
#[derive(Debug, Clone, Default)]
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
        self.open = true;
        self.tab = Some(SettingsTab::Connection);
        self.open_select = OpenSelect::None;
        self.pending = Some(PendingSettings::from_live(audio, settings));
    }

    pub fn close(&mut self) {
        self.open = false;
        self.tab = None;
        self.open_select = OpenSelect::None;
        self.pending = None;
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
    /// User clicked "Generate new identity"; close settings and open
    /// the identity wizard.
    OpenIdentityWizard,
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
    Dispatch(Vec<rumble_protocol::Command>),
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
const KEY_SAVE: &str = "settings:save";

const KEY_AUTOCONNECT: &str = "settings:conn:autoconnect";
const KEY_REGENERATE: &str = "settings:conn:regenerate";
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
/// `parse::<usize>()`. Pipelines never reorder behind the user's back
/// while the dialog is open, so the index is stable.
fn proc_enabled_key(idx: usize) -> String {
    format!("{PROC_PREFIX}{idx}:enabled")
}

/// Route key for one schema field of a processor. Layout:
/// `settings:proc:<idx>:f:<field>`. The `:f:` separator avoids
/// collisions with `enabled` even if a schema were ever to define a
/// field literally named `enabled`.
fn proc_field_key(idx: usize, field: &str) -> String {
    format!("{PROC_PREFIX}{idx}:f:{field}")
}

/// Inverse of [`proc_enabled_key`] / [`proc_field_key`]. Returns the
/// (processor index, optional field name) for routes under the
/// processing namespace, or `None` for unrelated routes.
fn parse_proc_route(route: &str) -> Option<(usize, Option<&str>)> {
    let rest = route.strip_prefix(PROC_PREFIX)?;
    let (idx_str, tail) = rest.split_once(':')?;
    let idx: usize = idx_str.parse().ok()?;
    if tail == "enabled" {
        return Some((idx, None));
    }
    let field = tail.strip_prefix("f:")?;
    Some((idx, Some(field)))
}

// ============================================================
// Render
// ============================================================

/// Render the settings dialog and any open dropdown popover. Returns
/// `(panel_layer, popover_layer)` so the App can compose them as
/// independent overlay layers — popovers must paint above the modal
/// panel they were anchored to. Returns `None` for the panel when the
/// dialog is closed.
pub fn render(
    state: &SettingsState,
    app_state: &State,
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
        SettingsTab::Devices => render_devices(pending, &app_state.audio),
        SettingsTab::Voice => render_voice(pending),
        SettingsTab::Processing => render_processing(pending, &app_state.audio, processor_registry, selection),
        SettingsTab::Sounds => render_sounds(pending),
        SettingsTab::Chat => render_chat(pending),
        SettingsTab::Shortcuts => render_shortcuts(pending, state, hotkeys),
        SettingsTab::Files => render_files(pending, selection),
        SettingsTab::Stats => render_stats(&app_state.audio),
        SettingsTab::Admin => admin::render(&state.admin, app_state, selection),
        SettingsTab::About => render_about(),
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

    let footer = row([
        button("Close").key(KEY_CLOSE),
        spacer(),
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

    let identity_lines: Vec<El> = if let Some(config) = identity.manager().config() {
        let (storage, detail) = match &config.source {
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
                let line = if comment.is_empty() {
                    format!("ssh-agent fingerprint: {agent_fp}")
                } else {
                    format!("ssh-agent: {comment} ({agent_fp})")
                };
                ("SSH agent", line)
            }
        };
        let path = identity.manager().config_dir().join("identity.json");
        let mut lines: Vec<El> = vec![
            field_row("Storage", text(storage.to_string()).semibold()),
            paragraph(detail).muted().font_size(tokens::TEXT_XS.size),
            field_row(
                "Fingerprint",
                mono(identity.fingerprint())
                    .font_size(tokens::TEXT_XS.size)
                    .ellipsis()
                    .width(Size::Fill(1.0)),
            ),
        ];
        if let Some(pubkey) = identity.public_key() {
            let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, pubkey);
            lines.push(field_row(
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
        lines.push(field_row(
            "On disk",
            mono(path.display().to_string())
                .font_size(tokens::TEXT_XS.size)
                .ellipsis()
                .width(Size::Fill(1.0)),
        ));
        lines
    } else {
        vec![paragraph("No identity configured.").text_color(tokens::WARNING)]
    };

    let mut children: Vec<El> = Vec::new();
    children.push(section_heading("Identity"));
    children.extend(identity_lines);
    children.push(
        alert([
            alert_title("Regenerating overwrites your identity"),
            alert_description(
                "Servers that knew the old key won't recognise the new one — you'll have to re-register or be \
                 re-approved.",
            ),
        ])
        .warning(),
    );
    children.push(
        row([
            spacer(),
            button("Generate new identity…").key(KEY_REGENERATE).destructive(),
        ])
        .width(Size::Fill(1.0)),
    );
    children.push(divider());
    children.push(field_row(
        "Autoconnect on launch",
        switch(pending.autoconnect).key(KEY_AUTOCONNECT),
    ));
    children.push(
        paragraph(
            "Reconnects to the most recently used server on startup. Effective once you've connected to at least one \
             server.",
        )
        .muted()
        .font_size(tokens::TEXT_XS.size),
    );

    if let Some(superuser) = render_superuser_section(app_state) {
        children.push(divider());
        children.push(section_heading("Superuser"));
        children.extend(superuser);
    }

    column(children).gap(tokens::SPACE_3).width(Size::Fill(1.0))
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

fn render_devices(pending: &PendingSettings, audio: &AudioState) -> El {
    let input_label = device_label_for(pending.input_device.as_deref(), &audio.input_devices);
    let output_label = device_label_for(pending.output_device.as_deref(), &audio.output_devices);

    column([
        section_heading("Input device"),
        select_trigger(KEY_INPUT_DEVICE, input_label),
        spacer().height(Size::Fixed(tokens::SPACE_1)),
        section_heading("Output device"),
        select_trigger(KEY_OUTPUT_DEVICE, output_label),
        spacer().height(Size::Fixed(tokens::SPACE_2)),
        row([button("Refresh devices").key(KEY_REFRESH_DEVICES).secondary(), spacer()]).width(Size::Fill(1.0)),
        divider(),
        paragraph(
            "Device changes apply when you click Save. Switching devices while connected may briefly drop audio.",
        )
        .muted()
        .font_size(tokens::TEXT_XS.size),
    ])
    .gap(tokens::SPACE_2)
    .width(Size::Fill(1.0))
}

fn render_voice(pending: &PendingSettings) -> El {
    let mode_slug = match pending.voice_mode {
        PersistentVoiceMode::PushToTalk => "ptt",
        PersistentVoiceMode::Continuous => "cont",
    };
    let bitrate_slug = bitrate_slug(pending.audio.bitrate);

    column([
        section_heading("Voice mode"),
        tabs_list(
            KEY_VOICE_MODE_TABS,
            &mode_slug,
            [("ptt", "Push-to-Talk"), ("cont", "Continuous")],
        ),
        paragraph(match pending.voice_mode {
            PersistentVoiceMode::PushToTalk => "Hold the configured PTT key (default: Space) to transmit.",
            PersistentVoiceMode::Continuous => "Always transmitting. Add a VAD processor to gate on voice activity.",
        })
        .muted()
        .font_size(tokens::TEXT_XS.size),
        divider(),
        section_heading("Encoder"),
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
        divider(),
        section_heading("Network"),
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
    ])
    .gap(tokens::SPACE_2)
    .width(Size::Fill(1.0))
}

fn render_processing(
    pending: &PendingSettings,
    audio: &AudioState,
    registry: &ProcessorRegistry,
    selection: &Selection,
) -> El {
    let mut rows: Vec<El> = Vec::new();
    rows.push(
        paragraph(
            "Audio processors applied to your microphone before encoding. Order is fixed; toggle individual stages \
             on/off.",
        )
        .muted()
        .font_size(tokens::TEXT_XS.size),
    );

    if pending.tx_pipeline.processors.is_empty() {
        rows.push(paragraph("No processors registered.").muted());
    }

    for (idx, proc_config) in pending.tx_pipeline.processors.iter().enumerate() {
        let display_name = registry
            .list_available()
            .into_iter()
            .find(|(type_id, _, _)| *type_id == proc_config.type_id.as_str())
            .map(|(_, name, _)| name.to_string())
            .unwrap_or_else(|| proc_config.type_id.clone());
        let description = registry
            .list_available()
            .into_iter()
            .find(|(type_id, _, _)| *type_id == proc_config.type_id.as_str())
            .map(|(_, _, desc)| desc.to_string())
            .unwrap_or_default();

        // Header row: processor name + enable switch. The description
        // sits on its own row underneath rather than next to the
        // switch, so the row's intrinsic height doesn't get clamped
        // to the switch and clip the wrapped description.
        rows.push(
            row([
                text(display_name).semibold(),
                spacer(),
                switch(proc_config.enabled).key(proc_enabled_key(idx)),
            ])
            .gap(tokens::SPACE_3)
            .align(Align::Center)
            .width(Size::Fill(1.0)),
        );
        if !description.is_empty() {
            rows.push(paragraph(description).muted().font_size(tokens::TEXT_XS.size));
        }

        // Per-processor schema fields (only when the processor is on).
        if proc_config.enabled
            && let Some(schema) = registry.settings_schema(&proc_config.type_id)
            && let Some(properties) = schema.get("properties").and_then(|p| p.as_object())
        {
            for (key, prop_schema) in properties {
                rows.push(render_schema_field(
                    idx,
                    key,
                    prop_schema,
                    &proc_config.settings,
                    selection,
                ));
            }
        }
        rows.push(spacer().height(Size::Fixed(tokens::SPACE_1)));
    }

    rows.push(divider());
    rows.push(section_heading("Input level"));
    rows.push(input_level_meter(&pending.tx_pipeline, audio));

    column(rows).gap(tokens::SPACE_2).width(Size::Fill(1.0))
}

/// Render one schema-defined property as a labelled control. `number` /
/// `integer` types become sliders (the only general-purpose continuous
/// control aetna ships out of the box), `boolean` becomes a switch,
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

/// Mumble-style VU meter that splits at the VAD threshold. Below the
/// threshold the bar paints in red ("won't transmit"); above the
/// threshold in green ("will transmit"). The break in colour follows
/// the threshold position, so dragging the threshold slider is
/// reflected immediately on the bar — that's the visual aid Mumble
/// users tune against.
///
/// When the VAD processor is disabled there's no threshold to break
/// against, so the meter falls back to a plain green/yellow/red
/// loudness display via [`progress`].
fn input_level_meter(pipeline: &PipelineConfig, audio: &AudioState) -> El {
    const MIN_DB: f32 = -60.0;
    const MAX_DB: f32 = 0.0;
    const SPAN_DB: f32 = MAX_DB - MIN_DB;
    const BAR_W: f32 = 280.0;
    const BAR_H: f32 = 14.0;

    let level_db = audio.input_level_db;
    let normalize = |db: f32| ((db - MIN_DB) / SPAN_DB).clamp(0.0, 1.0);
    let value_n = level_db.map(normalize).unwrap_or(0.0);

    let vad_threshold_db = pipeline
        .processors
        .iter()
        .find(|p| p.type_id == "builtin.vad" && p.enabled)
        .and_then(|p| p.settings.get("threshold_db"))
        .and_then(|v| v.as_f64())
        .map(|t| t as f32);

    let level_label = match level_db {
        Some(db) => format!("{db:.0} dB"),
        None => "—".to_string(),
    };
    let threshold_label = match vad_threshold_db {
        Some(db) => format!("VAD threshold: {db:.0} dB"),
        None => "VAD disabled — bar shows mic loudness only".to_string(),
    };

    let bar = match vad_threshold_db {
        Some(t_db) => threshold_bar(value_n, normalize(t_db), BAR_W, BAR_H),
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
                .width(Size::Fixed(BAR_W))
                .height(Size::Fixed(BAR_H))
        }
    };

    column([
        row([bar, text(level_label).mono().font_size(tokens::TEXT_XS.size)])
            .gap(tokens::SPACE_2)
            .align(Align::Center),
        text(threshold_label).muted().font_size(tokens::TEXT_XS.size),
    ])
    .gap(tokens::SPACE_1)
    .width(Size::Fill(1.0))
}

/// Four-segment bar mirroring `Mumble::AudioBar`: a darkened "won't
/// transmit" track from `0..threshold`, a darkened "will transmit"
/// track from `threshold..max`, plus brighter fills painted on top
/// proportional to the current level. The colour change at the
/// threshold position is the user's main visual cue when tuning the
/// VAD.
fn threshold_bar(value_n: f32, threshold_n: f32, bar_w: f32, bar_h: f32) -> El {
    let value_n = value_n.clamp(0.0, 1.0);
    let threshold_n = threshold_n.clamp(0.0, 1.0);

    let below_color = tokens::DESTRUCTIVE;
    let above_color = tokens::SUCCESS;
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
        vec![
            Rect::new(r.x, r.y, threshold_x, r.h),
            Rect::new(r.x + threshold_x, r.y, (r.w - threshold_x).max(0.0), r.h),
            Rect::new(r.x, r.y, below_fill_w, r.h),
            Rect::new(r.x + threshold_x, r.y, above_fill_w, r.h),
        ]
    };

    stack([
        El::new(Kind::Custom("vu-below-track")).fill(below_track),
        El::new(Kind::Custom("vu-above-track")).fill(above_track),
        El::new(Kind::Custom("vu-below-fill")).fill(below_color),
        El::new(Kind::Custom("vu-above-fill")).fill(above_color),
    ])
    .layout(layout)
    .width(Size::Fixed(bar_w))
    .height(Size::Fixed(bar_h))
}

fn render_sounds(pending: &PendingSettings) -> El {
    let mut rows: Vec<El> = Vec::new();
    rows.push(field_row(
        "Enable sound effects",
        switch(pending.sfx_enabled).key(KEY_SFX_ENABLED),
    ));
    rows.push(field_row(
        format!("Volume ({}%)", (pending.sfx_volume * 100.0).round() as i32),
        slider(pending.sfx_volume, tokens::PRIMARY)
            .key(KEY_SFX_VOLUME)
            .width(Size::Fixed(280.0)),
    ));
    rows.push(divider());
    rows.push(section_heading("Individual sounds"));
    for (idx, kind) in SfxKind::all().iter().enumerate() {
        let enabled = pending.sfx_kind_enabled.get(idx).copied().unwrap_or(true);
        rows.push(
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
    column(rows).gap(tokens::SPACE_2).width(Size::Fill(1.0))
}

fn render_chat(pending: &PendingSettings) -> El {
    column([
        field_row(
            "Show timestamps",
            switch(pending.show_timestamps).key(KEY_CHAT_SHOW_TIMESTAMPS),
        ),
        field_row(
            "Timestamp format",
            select_trigger(KEY_CHAT_FORMAT, pending.timestamp_format.label()).width(Size::Fixed(280.0)),
        ),
        divider(),
        field_row(
            "Auto-sync history on join",
            switch(pending.auto_sync_history).key(KEY_CHAT_AUTO_SYNC),
        ),
        paragraph("Asks peers for backlog when joining a room so you can read what was said before you arrived.")
            .muted()
            .font_size(tokens::TEXT_XS.size),
        divider(),
        field_row(
            "Auto-play animated images",
            switch(pending.gif_autoplay).key(KEY_CHAT_GIF_AUTOPLAY),
        ),
        paragraph("Off shows the first frame with a play button overlay until you start it.")
            .muted()
            .font_size(tokens::TEXT_XS.size),
    ])
    .gap(tokens::SPACE_2)
    .width(Size::Fill(1.0))
}

/// Mumble-style Shortcuts table. Header checkbox + columns
/// Function | Data | Shortcut. Function and Data are aetna select
/// dropdowns; the Shortcut cell is a button that either enters
/// capture mode (X11 / Windows / macOS) or opens the compositor's
/// system shortcut dialog (Wayland portal).
fn render_shortcuts(pending: &PendingSettings, state: &SettingsState, hotkeys: &HotkeyManager) -> El {
    let mut children: Vec<El> = Vec::new();

    children.push(field_row(
        "Enable global shortcuts",
        switch(pending.keyboard.global_hotkeys_enabled).key(KEY_SHORTCUTS_GLOBAL_ENABLE),
    ));

    let is_wayland = hotkeys.is_wayland();
    let helper = if is_wayland {
        "Wayland binds keys through the system shortcut settings — click any Shortcut cell to open it."
    } else {
        "Click a Shortcut cell to bind a key combination; press Escape to cancel capture."
    };
    children.push(paragraph(helper).muted().font_size(tokens::TEXT_XS.size));

    // Column header. Width budgets match the row body below so the
    // labels line up against the dropdown triggers / shortcut buttons.
    children.push(
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
        children.push(
            paragraph("No shortcuts configured — click Add to create one.")
                .muted()
                .font_size(tokens::TEXT_XS.size),
        );
    } else {
        for entry in &pending.keyboard.shortcuts {
            children.push(shortcut_row(entry, state, hotkeys, is_wayland));
        }
    }

    children.push(
        row([
            button_with_icon(IconName::Plus, "Add")
                .key(KEY_SHORTCUTS_ADD)
                .secondary(),
            spacer(),
        ])
        .gap(tokens::SPACE_2)
        .width(Size::Fill(1.0)),
    );

    if is_wayland {
        children.push(divider());
        children.push(section_heading("Use your desktop's shortcut settings (advanced)"));
        children.push(
            paragraph(
                "If the portal flow above doesn't suit your compositor (tiling WMs like Sway / Hyprland, or GNOME's \
                 Custom Shortcuts), bind any key in your desktop's keyboard settings to a shell command that talks to \
                 rumble over a Unix socket. Note: most desktop shortcut systems only fire on key press, so this path \
                 supports toggle / mute / deafen well but isn't ideal for hold-style push-to-talk.",
            )
            .muted()
            .font_size(tokens::TEXT_XS.size),
        );
        children.push(
            paragraph("Start aetna with `--rpc-server` to enable the socket at $XDG_RUNTIME_DIR/rumble/aetna.sock.")
                .muted()
                .font_size(tokens::TEXT_XS.size),
        );
        children.push(rpc_example_row("Toggle mute", RPC_EXAMPLE_TOGGLE_MUTE));
        children.push(rpc_example_row("Toggle deafen", RPC_EXAMPLE_TOGGLE_DEAFEN));
        children.push(rpc_example_row("Start transmit (PTT)", RPC_EXAMPLE_START_TRANSMIT));
        children.push(rpc_example_row("Stop transmit (PTT)", RPC_EXAMPLE_STOP_TRANSMIT));
    }

    column(children).gap(tokens::SPACE_2).width(Size::Fill(1.0))
}

/// Example shell commands the user can paste into their DE's custom-
/// shortcut configuration. `nc -U` is widely available and matches the
/// RPC server's newline-delimited JSON protocol.
const RPC_EXAMPLE_TOGGLE_MUTE: &str =
    r#"echo '{"method":"set_muted","muted":true}' | nc -UN "$XDG_RUNTIME_DIR/rumble/aetna.sock""#;
const RPC_EXAMPLE_TOGGLE_DEAFEN: &str =
    r#"echo '{"method":"set_deafened","deafened":true}' | nc -UN "$XDG_RUNTIME_DIR/rumble/aetna.sock""#;
const RPC_EXAMPLE_START_TRANSMIT: &str =
    r#"echo '{"method":"start_transmit"}' | nc -UN "$XDG_RUNTIME_DIR/rumble/aetna.sock""#;
const RPC_EXAMPLE_STOP_TRANSMIT: &str =
    r#"echo '{"method":"stop_transmit"}' | nc -UN "$XDG_RUNTIME_DIR/rumble/aetna.sock""#;

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
    let mut children: Vec<El> = vec![
        section_heading("Auto-download"),
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
        children.push(
            paragraph("No rules — add one below to start auto-downloading matching files.")
                .muted()
                .font_size(tokens::TEXT_XS.size),
        );
    } else {
        for (idx, rule) in pending.auto_download_rules.iter().enumerate() {
            children.push(rule_row(idx, rule, selection, pending.auto_download_enabled));
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
    children.push(row(action_row).gap(tokens::SPACE_2).width(Size::Fill(1.0)));

    children.push(divider());
    children.push(section_heading("Bandwidth limits"));
    children.push(field_row(
        format!("Download limit ({})", speed_label(pending.download_speed_kbps)),
        slider(
            pending.download_speed_kbps as f32 / MAX_SPEED_KBPS as f32,
            tokens::PRIMARY,
        )
        .key(KEY_FILES_DL_LIMIT)
        .width(Size::Fixed(280.0)),
    ));
    children.push(field_row(
        format!("Upload limit ({})", speed_label(pending.upload_speed_kbps)),
        slider(
            pending.upload_speed_kbps as f32 / MAX_SPEED_KBPS as f32,
            tokens::PRIMARY,
        )
        .key(KEY_FILES_UL_LIMIT)
        .width(Size::Fixed(280.0)),
    ));
    children.push(divider());
    children.push(section_heading("Download location"));
    children.push(download_dir_row(pending));
    children.push(
        paragraph(
            "Where downloaded files are saved. Changes apply on the next connect — active transfers keep using the \
             directory that was set when they started.",
        )
        .muted()
        .font_size(tokens::TEXT_XS.size),
    );

    column(children).gap(tokens::SPACE_2).width(Size::Fill(1.0))
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

fn render_stats(audio: &AudioState) -> El {
    let stats = &audio.stats;
    let loss_pct = stats.packet_loss_percent();
    let loss_color = if loss_pct > 5.0 {
        tokens::DESTRUCTIVE
    } else if loss_pct > 1.0 {
        tokens::WARNING
    } else {
        tokens::SUCCESS
    };

    column([
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
        stat_row("Packets sent", stats.packets_sent.to_string(), None),
        stat_row("Packets received", stats.packets_received.to_string(), None),
        stat_row(
            "Packet loss",
            format!("{:.1}% ({} lost)", loss_pct, stats.packets_lost),
            Some(loss_color),
        ),
        stat_row("FEC recovered", stats.packets_recovered_fec.to_string(), None),
        stat_row("Frames concealed", stats.frames_concealed.to_string(), None),
        stat_row(
            "Buffer level",
            format!("{} packets", stats.playback_buffer_packets),
            None,
        ),
        spacer().height(Size::Fixed(tokens::SPACE_2)),
        row([spacer(), button("Reset statistics").key(KEY_STATS_RESET).secondary()]).width(Size::Fill(1.0)),
    ])
    .gap(tokens::SPACE_1)
    .width(Size::Fill(1.0))
}

/// Build / version info populated by `build.rs` via `cargo:rustc-env=`.
/// Tag is empty when HEAD isn't tagged; dirty is "true" / "false".
fn render_about() -> El {
    let pkg_version = env!("CARGO_PKG_VERSION");
    let describe = env!("RUMBLE_AETNA_GIT_DESCRIBE");
    let short_hash = env!("RUMBLE_AETNA_GIT_SHORT_HASH");
    let full_hash = env!("RUMBLE_AETNA_GIT_HASH");
    let tag = env!("RUMBLE_AETNA_GIT_TAG");
    let dirty = env!("RUMBLE_AETNA_GIT_DIRTY") == "true";

    let mut rows: Vec<El> = Vec::new();
    rows.push(section_heading("Rumble"));
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

    column(rows).gap(tokens::SPACE_2).width(Size::Fill(1.0))
}

/// Convert an aetna `KeyPress` into a `HotkeyBinding` using the same
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

fn section_heading(label: impl Into<String>) -> El {
    text(label).semibold().font_size(tokens::TEXT_SM.size)
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

fn device_label_for(selected: Option<&str>, devices: &[rumble_protocol::AudioDeviceInfo]) -> String {
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
            // for any new variant aetna adds upstream.
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
    // Routes here are built at render time as `settings:proc:<idx>:enabled`
    // (the per-processor switch) or `settings:proc:<idx>:f:<field>` (a
    // schema-defined property). We dispatch text/switch/slider events
    // against whichever shape the schema picked, so the keyboard +
    // pointer codepaths converge into a single update of `pending.tx_pipeline`.

    // Text-input events arrive on `target_key` rather than `route`, so
    // check this before the generic `route()`-driven handlers.
    if let Some(target) = event.target_key()
        && let Some((idx, Some(field))) = parse_proc_route(target)
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
        let Some(proc_config) = pending.tx_pipeline.processors.get_mut(idx) else {
            return SettingsOutcome::Ignored;
        };

        match slot {
            // Per-processor enable switch.
            None => {
                if switch::apply_event(&mut proc_config.enabled, event, route) {
                    return SettingsOutcome::Handled;
                }
            }
            // Schema-driven field. The schema lookup is the source of
            // truth for "what control type was rendered" — the event
            // codepaths can't peek at the rendered tree.
            Some(field) => {
                let prop_type = schema_field_type(processor_registry, &proc_config.type_id, field);
                let prop_schema = processor_registry
                    .settings_schema(&proc_config.type_id)
                    .and_then(|s| s.get("properties").cloned())
                    .and_then(|p| p.get(field).cloned());

                match prop_type {
                    Some("boolean") => {
                        let mut value = proc_config
                            .settings
                            .get(field)
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
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
