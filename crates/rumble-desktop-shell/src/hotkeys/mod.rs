//! Global hotkey service for the desktop client.
//!
//! Three platform backends, all hidden behind one `HotkeyManager` API:
//! - **Windows / macOS / Linux X11** via `global-hotkey` (the
//!   `global-hotkeys` feature).
//! - **Linux Wayland** via the XDG GlobalShortcuts portal (the
//!   `wayland-portal` feature). The portal is contacted asynchronously
//!   from a tokio runtime, so callers pass a `tokio::runtime::Handle`
//!   to `init_portal_backend`.
//! - **Anywhere else** the manager exists but is dormant — callers can
//!   still query `is_available()`, `is_wayland()`, etc. without `cfg`.

use std::{collections::HashMap, sync::Arc};

use serde::{Deserialize, Serialize};

#[cfg(feature = "global-hotkeys")]
use global_hotkey::{
    GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState,
    hotkey::{Code, HotKey, Modifiers},
};

#[cfg(all(target_os = "linux", feature = "wayland-portal"))]
pub mod portal;

#[cfg(all(target_os = "linux", feature = "wayland-portal"))]
use portal::PortalHotkeyBackend;

/// Callback poked whenever a hotkey event is enqueued from a backend
/// thread. The GUI passes its repaint/wakeup hook here so a global
/// shortcut schedules a frame immediately — the consumer side
/// (`poll_events`) only runs during frame production, and with
/// push-driven redraws nothing else would wake the event loop while
/// the window is idle (and likely unfocused, this being a *global*
/// hotkey).
pub type HotkeyWakeup = Arc<dyn Fn() + Send + Sync>;

/// Persisted keyboard configuration. Lives in the hotkey module rather
/// than `settings::Settings` because the hotkey UI editor reads/writes
/// this directly and the types travel together.
///
/// Bindings are stored as a flat `Vec<ShortcutEntry>` so the user can
/// add multiple rows per function (multiple PTT keys, separate
/// MuteSelf+On / MuteSelf+Off rows). Each entry has a stable `id` used
/// both as the XDG portal shortcut ID (so the compositor preserves the
/// user's key assignment across re-binds) and as the UI row key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct KeyboardSettings {
    /// Whether global hotkeys are enabled (work when window unfocused).
    pub global_hotkeys_enabled: bool,
    /// User-configured shortcut rows. Order is preserved for stable UI
    /// rendering; ordering carries no semantic meaning.
    pub shortcuts: Vec<ShortcutEntry>,

    // Legacy single-binding fields. Read on load and drained into
    // `shortcuts` by `normalize_legacy()` so old configs migrate
    // transparently; skip-serialized so they disappear on next save.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "ptt_hotkey")]
    legacy_ptt: Option<HotkeyBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "toggle_mute_hotkey")]
    legacy_toggle_mute: Option<HotkeyBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "toggle_deafen_hotkey")]
    legacy_toggle_deafen: Option<HotkeyBinding>,
}

impl Default for KeyboardSettings {
    fn default() -> Self {
        // Empty shortcuts list. There's deliberately no seeded "Space →
        // push-to-talk" entry — auto-binding a global hotkey can collide
        // with the user's existing keymap, and on Wayland it would fire
        // the XDG portal prompt at app startup. Users add their own rows
        // from the Shortcuts settings tab.
        Self {
            global_hotkeys_enabled: true,
            shortcuts: Vec::new(),
            legacy_ptt: None,
            legacy_toggle_mute: None,
            legacy_toggle_deafen: None,
        }
    }
}

impl KeyboardSettings {
    /// Drain legacy single-binding fields into `shortcuts`. Idempotent:
    /// subsequent calls find the legacy fields already `None` and do
    /// nothing. Each legacy field appends a new entry — if `shortcuts`
    /// already contained a row for the same (function, data) pair, both
    /// survive (the user gets the union, which they can prune in the UI).
    pub fn normalize_legacy(&mut self) {
        for (legacy, function, data) in [
            (self.legacy_ptt.take(), HotkeyFunction::PushToTalk, HotkeyData::Hold),
            (
                self.legacy_toggle_mute.take(),
                HotkeyFunction::MuteSelf,
                HotkeyData::Toggle,
            ),
            (
                self.legacy_toggle_deafen.take(),
                HotkeyFunction::DeafenSelf,
                HotkeyData::Toggle,
            ),
        ] {
            if let Some(binding) = legacy {
                self.shortcuts.push(ShortcutEntry {
                    id: ShortcutEntry::new_id(),
                    function,
                    data,
                    binding: Some(binding),
                });
            }
        }
    }

    /// First binding configured for `(function, data)`, if any. Useful
    /// for callers that conceptually only care about one binding per
    /// action (e.g. a keyboard-shortcuts settings panel).
    pub fn primary_binding(&self, function: HotkeyFunction, data: HotkeyData) -> Option<&HotkeyBinding> {
        self.shortcuts
            .iter()
            .find(|s| s.function == function && s.data == data)
            .and_then(|s| s.binding.as_ref())
    }

    /// Set / clear the first entry matching `(function, data)`. Inserts
    /// a new entry if none exists; clears `binding` when called with
    /// `None`. Matches the historical `keyboard.ptt_hotkey = Some(b)`
    /// assignment pattern but routes through the unified vec.
    pub fn set_primary_binding(&mut self, function: HotkeyFunction, data: HotkeyData, binding: Option<HotkeyBinding>) {
        if let Some(slot) = self
            .shortcuts
            .iter_mut()
            .find(|s| s.function == function && s.data == data)
        {
            slot.binding = binding;
            return;
        }
        self.shortcuts.push(ShortcutEntry {
            id: ShortcutEntry::new_id(),
            function,
            data,
            binding,
        });
    }
}

/// The action a shortcut performs. Future additions are appended; the
/// JSON tag must stay stable for serialised settings to roundtrip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HotkeyFunction {
    PushToTalk,
    MuteSelf,
    DeafenSelf,
}

impl HotkeyFunction {
    pub const ALL: &'static [HotkeyFunction] = &[
        HotkeyFunction::PushToTalk,
        HotkeyFunction::MuteSelf,
        HotkeyFunction::DeafenSelf,
    ];

    pub fn label(self) -> &'static str {
        match self {
            HotkeyFunction::PushToTalk => "Push-to-Talk",
            HotkeyFunction::MuteSelf => "Mute Self",
            HotkeyFunction::DeafenSelf => "Deafen Self",
        }
    }

    /// Stable token used in routed-event keys and serde tags.
    pub fn slug(self) -> &'static str {
        match self {
            HotkeyFunction::PushToTalk => "ptt",
            HotkeyFunction::MuteSelf => "mute-self",
            HotkeyFunction::DeafenSelf => "deafen-self",
        }
    }

    pub fn from_slug(slug: &str) -> Option<HotkeyFunction> {
        Self::ALL.iter().copied().find(|f| f.slug() == slug)
    }

    /// Which `HotkeyData` variants make sense for this function. PTT is
    /// hold-only; mute / deafen toggle / on / off.
    pub fn data_options(self) -> &'static [HotkeyData] {
        match self {
            HotkeyFunction::PushToTalk => &[HotkeyData::Hold],
            HotkeyFunction::MuteSelf | HotkeyFunction::DeafenSelf => {
                &[HotkeyData::Toggle, HotkeyData::On, HotkeyData::Off]
            }
        }
    }

    /// Sensible `HotkeyData` to pick when the user first chooses this
    /// function. Used by the "Add" button to seed a new row.
    pub fn default_data(self) -> HotkeyData {
        self.data_options()[0]
    }
}

/// Modal sub-action for a function — what *kind* of mute (toggle / on /
/// off) or the hold semantics for PTT.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HotkeyData {
    /// Press-and-hold; fires `Released` events. PTT uses this.
    Hold,
    /// Single press flips state.
    Toggle,
    /// Single press sets state to on.
    On,
    /// Single press sets state to off.
    Off,
}

impl HotkeyData {
    pub fn label(self) -> &'static str {
        match self {
            HotkeyData::Hold => "Hold",
            HotkeyData::Toggle => "Toggle",
            HotkeyData::On => "On",
            HotkeyData::Off => "Off",
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            HotkeyData::Hold => "hold",
            HotkeyData::Toggle => "toggle",
            HotkeyData::On => "on",
            HotkeyData::Off => "off",
        }
    }

    pub fn from_slug(slug: &str) -> Option<HotkeyData> {
        match slug {
            "hold" => Some(HotkeyData::Hold),
            "toggle" => Some(HotkeyData::Toggle),
            "on" => Some(HotkeyData::On),
            "off" => Some(HotkeyData::Off),
            _ => None,
        }
    }
}

/// One configurable shortcut row. The `id` is stable across edits so
/// the XDG portal preserves the user's key assignment when we re-bind
/// after add / remove / function change. UUID-string format to give
/// the system shortcut dialog something readable when no human label
/// is available.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShortcutEntry {
    pub id: String,
    pub function: HotkeyFunction,
    pub data: HotkeyData,
    /// Currently bound key combo (X11/Win/macOS backend) or `None`
    /// when the row exists but no key is assigned. On Wayland the
    /// portal owns key assignment, so this stays `None`; the displayed
    /// trigger comes from `HotkeyManager::portal_trigger(id)`.
    pub binding: Option<HotkeyBinding>,
}

impl ShortcutEntry {
    pub fn new_id() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    /// Description string passed to the XDG portal at bind time. Shows
    /// up in the user's compositor shortcut settings dialog, so make
    /// it human-readable.
    pub fn portal_description(&self) -> String {
        format!("{} ({})", self.function.label(), self.data.label())
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

/// Registration status for a single configured shortcut entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyRegistrationStatus {
    /// Hotkey registered successfully.
    Registered,
    /// Registration failed.
    Failed,
    /// No binding configured for this entry.
    NotConfigured,
}

/// Result of polling the manager. The (function, data) pair describes
/// which configured row fired; press / release semantics let callers
/// distinguish push-to-talk (Hold) from one-shot actions.
#[derive(Debug, Clone, Copy)]
pub enum HotkeyEvent {
    Pressed { function: HotkeyFunction, data: HotkeyData },
    Released { function: HotkeyFunction, data: HotkeyData },
}

/// Push-side plumbing for the `global-hotkey` backend. The crate
/// delivers events either to a static poll channel **or** to a
/// process-global handler installed via
/// `GlobalHotKeyEvent::set_event_handler` — never both, and the handler
/// can only ever be installed once per process. We install a handler
/// (so event arrival can poke the GUI wakeup) that forwards into this
/// shared queue, which `poll_events` drains in place of the crate's
/// channel.
#[cfg(feature = "global-hotkeys")]
mod gh_push {
    use std::{
        collections::VecDeque,
        sync::{Mutex, OnceLock},
    };

    use global_hotkey::GlobalHotKeyEvent;

    use super::HotkeyWakeup;

    pub(super) static QUEUE: Mutex<VecDeque<GlobalHotKeyEvent>> = Mutex::new(VecDeque::new());
    pub(super) static WAKEUP: Mutex<Option<HotkeyWakeup>> = Mutex::new(None);
    static INSTALLED: OnceLock<()> = OnceLock::new();

    pub(super) fn install_handler() {
        INSTALLED.get_or_init(|| {
            GlobalHotKeyEvent::set_event_handler(Some(|event: GlobalHotKeyEvent| {
                QUEUE.lock().unwrap().push_back(event);
                let wakeup = WAKEUP.lock().unwrap().clone();
                if let Some(wakeup) = wakeup {
                    wakeup();
                }
            }));
        });
    }
}

/// Manages global hotkey registration and events.
///
/// On macOS this MUST be created on the main thread (a `global-hotkey`
/// requirement that bubbles up to us).
pub struct HotkeyManager {
    #[cfg(feature = "global-hotkeys")]
    manager: Option<GlobalHotKeyManager>,
    /// Map from registered hotkey id to the configured row that fired it.
    #[cfg(feature = "global-hotkeys")]
    hotkey_actions: HashMap<u32, (HotkeyFunction, HotkeyData)>,
    /// Registered hotkeys (held so we can unregister cleanly on drop).
    #[cfg(feature = "global-hotkeys")]
    registered_hotkeys: Vec<HotKey>,
    /// Per-entry registration status, keyed by `ShortcutEntry::id`.
    /// Drives the row status dot in the settings UI.
    registration_status: HashMap<String, HotkeyRegistrationStatus>,
    is_wayland: bool,
    /// Set when the underlying global-hotkey manager refused to init
    /// for reasons other than "we're on Wayland". Surfaces in the UI.
    init_failed: bool,
    /// Tracks whether global hotkeys are currently enabled per user
    /// settings. Updated at the top of every `register_from_settings`
    /// call; gates portal event dispatch in `poll_events` to cover the
    /// async window between `release_all` spawning and the action map
    /// being cleared inside the spawned task.
    global_hotkeys_enabled: bool,
    /// Wakeup hook from [`Self::set_wakeup`], retained so a portal
    /// backend created later (`init_portal_backend` runs after the
    /// manager is constructed) still inherits it.
    #[cfg(all(target_os = "linux", feature = "wayland-portal"))]
    wakeup: Option<HotkeyWakeup>,
    #[cfg(all(target_os = "linux", feature = "wayland-portal"))]
    portal_backend: Option<PortalHotkeyBackend>,
}

impl HotkeyManager {
    pub fn new() -> Self {
        let is_wayland = Self::detect_wayland();

        // On Wayland, skip the global-hotkey manager entirely — X11
        // grab APIs aren't reachable. Portal backend (if compiled) is
        // initialised separately via `init_portal_backend`.
        if is_wayland {
            tracing::info!("Running on Wayland - will use XDG Portal for global shortcuts if available");
            return Self {
                #[cfg(feature = "global-hotkeys")]
                manager: None,
                #[cfg(feature = "global-hotkeys")]
                hotkey_actions: HashMap::new(),
                #[cfg(feature = "global-hotkeys")]
                registered_hotkeys: Vec::new(),
                registration_status: HashMap::new(),
                is_wayland: true,
                init_failed: false,
                global_hotkeys_enabled: true,
                #[cfg(all(target_os = "linux", feature = "wayland-portal"))]
                wakeup: None,
                #[cfg(all(target_os = "linux", feature = "wayland-portal"))]
                portal_backend: None,
            };
        }

        #[cfg(feature = "global-hotkeys")]
        {
            match GlobalHotKeyManager::new() {
                Ok(manager) => {
                    tracing::info!("Global hotkey manager initialized successfully");
                    Self {
                        manager: Some(manager),
                        hotkey_actions: HashMap::new(),
                        registered_hotkeys: Vec::new(),
                        registration_status: HashMap::new(),
                        is_wayland: false,
                        init_failed: false,
                        global_hotkeys_enabled: true,
                        #[cfg(all(target_os = "linux", feature = "wayland-portal"))]
                        wakeup: None,
                        #[cfg(all(target_os = "linux", feature = "wayland-portal"))]
                        portal_backend: None,
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to create GlobalHotKeyManager: {}", e);
                    Self {
                        manager: None,
                        hotkey_actions: HashMap::new(),
                        registered_hotkeys: Vec::new(),
                        registration_status: HashMap::new(),
                        is_wayland: false,
                        init_failed: true,
                        global_hotkeys_enabled: true,
                        #[cfg(all(target_os = "linux", feature = "wayland-portal"))]
                        wakeup: None,
                        #[cfg(all(target_os = "linux", feature = "wayland-portal"))]
                        portal_backend: None,
                    }
                }
            }
        }

        #[cfg(not(feature = "global-hotkeys"))]
        Self {
            registration_status: HashMap::new(),
            is_wayland: false,
            init_failed: false,
            global_hotkeys_enabled: true,
            #[cfg(all(target_os = "linux", feature = "wayland-portal"))]
            wakeup: None,
            #[cfg(all(target_os = "linux", feature = "wayland-portal"))]
            portal_backend: None,
        }
    }

    /// Initialize the XDG Portal backend for Wayland global shortcuts.
    /// Call once after the tokio runtime is available. No-op outside
    /// Linux/Wayland.
    #[cfg(all(target_os = "linux", feature = "wayland-portal"))]
    pub async fn init_portal_backend(&mut self, runtime_handle: tokio::runtime::Handle) {
        if !self.is_wayland {
            return;
        }
        // Test/headless escape hatch — `crates/rumble-damascene/src/bin/dump_bundles.rs`
        // builds many RumbleApp instances back-to-back, and each portal
        // session leaves a D-Bus listener task behind that backs up
        // xdg-desktop-portal until the dump hangs mid-run. Setting this
        // env var lets headless tools opt out without faking a desktop
        // session type.
        if std::env::var_os("RUMBLE_DISABLE_PORTAL").is_some() {
            return;
        }
        tracing::info!("Initializing XDG Portal GlobalShortcuts backend");
        match PortalHotkeyBackend::new(runtime_handle).await {
            Some(backend) => {
                tracing::info!("XDG Portal GlobalShortcuts session created (shortcuts bind lazily on settings load)");
                if let Some(wakeup) = &self.wakeup {
                    backend.set_wakeup(wakeup.clone());
                }
                self.portal_backend = Some(backend);
            }
            None => {
                tracing::warn!("XDG Portal GlobalShortcuts not available — falling back to window-focused shortcuts");
            }
        }
    }

    /// No-op fallback when the wayland-portal feature or platform is
    /// unavailable. Lets call sites stay `cfg`-free.
    #[cfg(not(all(target_os = "linux", feature = "wayland-portal")))]
    pub async fn init_portal_backend(&mut self, _runtime_handle: tokio::runtime::Handle) {}

    /// Install a callback poked whenever a hotkey event arrives from a
    /// backend thread, so an event-driven GUI can schedule a frame (and
    /// thus a `poll_events` drain) without waiting for unrelated input.
    /// Without this, a push-redraw GUI sits idle while events queue up —
    /// global hotkeys mostly fire when the window is unfocused, so no
    /// winit event is coming to flush them.
    ///
    /// Call order with `init_portal_backend` doesn't matter; the hook is
    /// retained and handed to a portal backend created later.
    pub fn set_wakeup(&mut self, wakeup: HotkeyWakeup) {
        #[cfg(all(target_os = "linux", feature = "wayland-portal"))]
        {
            if let Some(portal) = &self.portal_backend {
                portal.set_wakeup(wakeup.clone());
            }
            self.wakeup = Some(wakeup.clone());
        }
        #[cfg(feature = "global-hotkeys")]
        if self.manager.is_some() {
            *gh_push::WAKEUP.lock().unwrap() = Some(wakeup.clone());
            gh_push::install_handler();
        }
        let _ = wakeup;
    }

    /// Whether the portal-based shortcut backend is bound.
    pub fn has_portal_backend(&self) -> bool {
        #[cfg(all(target_os = "linux", feature = "wayland-portal"))]
        {
            self.portal_backend.as_ref().map(|b| b.is_available()).unwrap_or(false)
        }
        #[cfg(not(all(target_os = "linux", feature = "wayland-portal")))]
        {
            false
        }
    }

    /// Get the current portal shortcut bindings (Linux/Wayland only).
    #[cfg(all(target_os = "linux", feature = "wayland-portal"))]
    pub fn get_portal_shortcuts(&self) -> Vec<portal::ShortcutInfo> {
        self.portal_backend
            .as_ref()
            .map(|b| b.get_shortcuts())
            .unwrap_or_default()
    }

    /// Get the current portal shortcut bindings (always empty without portal).
    #[cfg(not(all(target_os = "linux", feature = "wayland-portal")))]
    pub fn get_portal_shortcuts(&self) -> Vec<()> {
        Vec::new()
    }

    /// Open the system shortcut configuration dialog (Linux/Wayland only).
    #[cfg(all(target_os = "linux", feature = "wayland-portal"))]
    pub fn open_portal_settings(&self) {
        if let Some(ref backend) = self.portal_backend {
            backend.open_settings();
        }
    }

    /// Open the system shortcut configuration dialog (no-op without portal).
    #[cfg(not(all(target_os = "linux", feature = "wayland-portal")))]
    pub fn open_portal_settings(&self) {}

    /// Cleanly tear down the XDG portal D-Bus session before the owning
    /// tokio runtime is dropped.
    ///
    /// `runtime` must be the same runtime the portal backend was
    /// initialised against. The portal's zbus connection spawns onto the
    /// tokio executor while closing, so dropping it on the main thread
    /// during normal field teardown — after the runtime has already gone
    /// away — panics with "there is no reactor running". Driving the drop
    /// through `block_on` keeps a reactor in scope. No-op on non-portal
    /// builds.
    #[cfg(all(target_os = "linux", feature = "wayland-portal"))]
    pub fn shutdown_portal(&mut self, runtime: &tokio::runtime::Runtime) {
        if let Some(backend) = self.portal_backend.take() {
            runtime.block_on(backend.shutdown());
        }
    }

    /// No-op portal shutdown for non-portal builds.
    #[cfg(not(all(target_os = "linux", feature = "wayland-portal")))]
    pub fn shutdown_portal(&mut self, _runtime: &tokio::runtime::Runtime) {}

    fn detect_wayland() -> bool {
        #[cfg(target_os = "linux")]
        {
            std::env::var("XDG_SESSION_TYPE")
                .map(|v| v.to_lowercase() == "wayland")
                .unwrap_or(false)
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    pub fn is_available(&self) -> bool {
        #[cfg(feature = "global-hotkeys")]
        {
            self.manager.is_some()
        }
        #[cfg(not(feature = "global-hotkeys"))]
        {
            false
        }
    }

    pub fn is_wayland(&self) -> bool {
        self.is_wayland
    }

    pub fn init_failed(&self) -> bool {
        self.init_failed
    }

    pub fn get_registration_status(&self, entry_id: &str) -> HotkeyRegistrationStatus {
        self.registration_status
            .get(entry_id)
            .copied()
            .unwrap_or(HotkeyRegistrationStatus::NotConfigured)
    }

    pub fn registration_status_map(&self) -> &HashMap<String, HotkeyRegistrationStatus> {
        &self.registration_status
    }

    /// Reconcile registered hotkeys with `settings`. Always re-registers
    /// from scratch — diffing per-row is fiddly and the operation is
    /// rare (only when the user touches the shortcuts UI). On Wayland
    /// the portal backend is re-bound with the current shortcut
    /// descriptions; the compositor preserves the user's key assignment
    /// against stable entry IDs.
    pub fn register_from_settings(&mut self, settings: &KeyboardSettings) -> Result<(), String> {
        self.unregister_all();
        self.registration_status.clear();

        // Seed every configured row as NotConfigured so the UI can
        // render a stable status grid even when bindings are missing.
        for entry in &settings.shortcuts {
            self.registration_status
                .insert(entry.id.clone(), HotkeyRegistrationStatus::NotConfigured);
        }

        // Track enabled state before the portal block so that
        // poll_events gates correctly the moment this function returns,
        // even before the async release task has cleared the action map.
        self.global_hotkeys_enabled = settings.global_hotkeys_enabled;

        #[cfg(all(target_os = "linux", feature = "wayland-portal"))]
        if let Some(ref mut portal) = self.portal_backend {
            if settings.global_hotkeys_enabled {
                portal.set_shortcuts(&settings.shortcuts);
                for entry in &settings.shortcuts {
                    // Portal-backed registration: the compositor owns key
                    // assignment, so we don't know yet whether a trigger
                    // exists. Treat the entry as Registered (i.e. the
                    // portal knows about it); the UI surfaces the actual
                    // trigger string separately.
                    self.registration_status
                        .insert(entry.id.clone(), HotkeyRegistrationStatus::Registered);
                }
            } else {
                // Release compositor-side grabs without closing the
                // session so that re-enabling can rebind on the same
                // session via a subsequent set_shortcuts call.
                portal.release_all();
            }
        }

        if !settings.global_hotkeys_enabled {
            tracing::info!("Global hotkeys disabled in settings");
            return Ok(());
        }

        #[cfg(feature = "global-hotkeys")]
        {
            let Some(ref manager) = self.manager else {
                return Ok(());
            };

            for entry in &settings.shortcuts {
                let Some(binding) = entry.binding.as_ref() else {
                    continue;
                };
                match Self::binding_to_hotkey(binding) {
                    Ok(hotkey) => {
                        let id = hotkey.id();
                        if let Err(e) = manager.register(hotkey) {
                            tracing::warn!("Failed to register {:?}/{:?}: {e}", entry.function, entry.data);
                            self.registration_status
                                .insert(entry.id.clone(), HotkeyRegistrationStatus::Failed);
                        } else {
                            self.hotkey_actions.insert(id, (entry.function, entry.data));
                            self.registered_hotkeys.push(hotkey);
                            self.registration_status
                                .insert(entry.id.clone(), HotkeyRegistrationStatus::Registered);
                            tracing::info!(
                                "Registered {:?}/{:?}: {}",
                                entry.function,
                                entry.data,
                                binding.display()
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Invalid {:?}/{:?} binding: {e}", entry.function, entry.data);
                        self.registration_status
                            .insert(entry.id.clone(), HotkeyRegistrationStatus::Failed);
                    }
                }
            }
        }

        Ok(())
    }

    /// Drop every active registration. Safe to call repeatedly.
    pub fn unregister_all(&mut self) {
        #[cfg(feature = "global-hotkeys")]
        {
            if let Some(ref manager) = self.manager {
                for hotkey in &self.registered_hotkeys {
                    let _ = manager.unregister(*hotkey);
                }
            }
            self.hotkey_actions.clear();
            self.registered_hotkeys.clear();
        }
    }

    /// Drain pending events from every backend. Returns ordered events
    /// (portal first, then global-hotkey) — order matters when a single
    /// physical key triggers both backends in close succession.
    pub fn poll_events(&mut self) -> Vec<HotkeyEvent> {
        let mut events = Vec::new();

        #[cfg(all(target_os = "linux", feature = "wayland-portal"))]
        if let Some(ref mut portal) = self.portal_backend {
            let portal_events = portal.poll_events();
            // Always drain to prevent channel build-up; only dispatch
            // when the feature is active. This closes the async window
            // between release_all spawning and the action map clearing —
            // any compositor signals that slip through during that window
            // are silently discarded here rather than delivered to callers.
            if self.global_hotkeys_enabled {
                events.extend(portal_events);
            }
        }

        #[cfg(feature = "global-hotkeys")]
        if self.manager.is_some() {
            let mut dispatch = |event: GlobalHotKeyEvent| {
                if let Some(&(function, data)) = self.hotkey_actions.get(&event.id) {
                    match event.state {
                        HotKeyState::Pressed => events.push(HotkeyEvent::Pressed { function, data }),
                        HotKeyState::Released => events.push(HotkeyEvent::Released { function, data }),
                    }
                }
            };
            // Push handler queue (installed by `set_wakeup`). Drained
            // first: any events the crate's channel still holds predate
            // the handler install.
            for event in gh_push::QUEUE.lock().unwrap().drain(..) {
                dispatch(event);
            }
            // Crate poll channel — only fed while no push handler is
            // installed (i.e. `set_wakeup` was never called).
            while let Ok(event) = GlobalHotKeyEvent::receiver().try_recv() {
                dispatch(event);
            }
        }

        events
    }

    /// Portal-side trigger description for the given shortcut entry id,
    /// if one is bound. Empty when the user hasn't assigned a key in
    /// system settings. Always `None` outside Wayland.
    pub fn portal_trigger(&self, _entry_id: &str) -> Option<String> {
        #[cfg(all(target_os = "linux", feature = "wayland-portal"))]
        {
            self.portal_backend.as_ref().and_then(|b| b.trigger_for(_entry_id))
        }
        #[cfg(not(all(target_os = "linux", feature = "wayland-portal")))]
        {
            None
        }
    }

    #[cfg(feature = "global-hotkeys")]
    fn binding_to_hotkey(binding: &HotkeyBinding) -> Result<HotKey, String> {
        let modifiers = Self::modifiers_to_global(&binding.modifiers);
        let code = Self::key_string_to_code(&binding.key)?;
        Ok(HotKey::new(modifiers, code))
    }

    #[cfg(feature = "global-hotkeys")]
    fn modifiers_to_global(mods: &HotkeyModifiers) -> Option<Modifiers> {
        let mut result = Modifiers::empty();
        if mods.ctrl {
            result |= Modifiers::CONTROL;
        }
        if mods.shift {
            result |= Modifiers::SHIFT;
        }
        if mods.alt {
            result |= Modifiers::ALT;
        }
        if mods.super_key {
            result |= Modifiers::SUPER;
        }
        if result.is_empty() { None } else { Some(result) }
    }

    /// Convert key string to global-hotkey `Code`.
    #[cfg(feature = "global-hotkeys")]
    pub fn key_string_to_code(key: &str) -> Result<Code, String> {
        match key.to_lowercase().as_str() {
            "space" => Ok(Code::Space),
            "enter" | "return" => Ok(Code::Enter),
            "tab" => Ok(Code::Tab),
            "backspace" => Ok(Code::Backspace),
            "escape" | "esc" => Ok(Code::Escape),
            "insert" => Ok(Code::Insert),
            "delete" => Ok(Code::Delete),
            "home" => Ok(Code::Home),
            "end" => Ok(Code::End),
            "pageup" => Ok(Code::PageUp),
            "pagedown" => Ok(Code::PageDown),

            "arrowup" | "up" => Ok(Code::ArrowUp),
            "arrowdown" | "down" => Ok(Code::ArrowDown),
            "arrowleft" | "left" => Ok(Code::ArrowLeft),
            "arrowright" | "right" => Ok(Code::ArrowRight),

            "f1" => Ok(Code::F1),
            "f2" => Ok(Code::F2),
            "f3" => Ok(Code::F3),
            "f4" => Ok(Code::F4),
            "f5" => Ok(Code::F5),
            "f6" => Ok(Code::F6),
            "f7" => Ok(Code::F7),
            "f8" => Ok(Code::F8),
            "f9" => Ok(Code::F9),
            "f10" => Ok(Code::F10),
            "f11" => Ok(Code::F11),
            "f12" => Ok(Code::F12),

            "a" | "keya" => Ok(Code::KeyA),
            "b" | "keyb" => Ok(Code::KeyB),
            "c" | "keyc" => Ok(Code::KeyC),
            "d" | "keyd" => Ok(Code::KeyD),
            "e" | "keye" => Ok(Code::KeyE),
            "f" | "keyf" => Ok(Code::KeyF),
            "g" | "keyg" => Ok(Code::KeyG),
            "h" | "keyh" => Ok(Code::KeyH),
            "i" | "keyi" => Ok(Code::KeyI),
            "j" | "keyj" => Ok(Code::KeyJ),
            "k" | "keyk" => Ok(Code::KeyK),
            "l" | "keyl" => Ok(Code::KeyL),
            "m" | "keym" => Ok(Code::KeyM),
            "n" | "keyn" => Ok(Code::KeyN),
            "o" | "keyo" => Ok(Code::KeyO),
            "p" | "keyp" => Ok(Code::KeyP),
            "q" | "keyq" => Ok(Code::KeyQ),
            "r" | "keyr" => Ok(Code::KeyR),
            "s" | "keys" => Ok(Code::KeyS),
            "t" | "keyt" => Ok(Code::KeyT),
            "u" | "keyu" => Ok(Code::KeyU),
            "v" | "keyv" => Ok(Code::KeyV),
            "w" | "keyw" => Ok(Code::KeyW),
            "x" | "keyx" => Ok(Code::KeyX),
            "y" | "keyy" => Ok(Code::KeyY),
            "z" | "keyz" => Ok(Code::KeyZ),

            "0" | "digit0" => Ok(Code::Digit0),
            "1" | "digit1" => Ok(Code::Digit1),
            "2" | "digit2" => Ok(Code::Digit2),
            "3" | "digit3" => Ok(Code::Digit3),
            "4" | "digit4" => Ok(Code::Digit4),
            "5" | "digit5" => Ok(Code::Digit5),
            "6" | "digit6" => Ok(Code::Digit6),
            "7" | "digit7" => Ok(Code::Digit7),
            "8" | "digit8" => Ok(Code::Digit8),
            "9" | "digit9" => Ok(Code::Digit9),

            "numpad0" => Ok(Code::Numpad0),
            "numpad1" => Ok(Code::Numpad1),
            "numpad2" => Ok(Code::Numpad2),
            "numpad3" => Ok(Code::Numpad3),
            "numpad4" => Ok(Code::Numpad4),
            "numpad5" => Ok(Code::Numpad5),
            "numpad6" => Ok(Code::Numpad6),
            "numpad7" => Ok(Code::Numpad7),
            "numpad8" => Ok(Code::Numpad8),
            "numpad9" => Ok(Code::Numpad9),
            "numpadadd" => Ok(Code::NumpadAdd),
            "numpadsubtract" => Ok(Code::NumpadSubtract),
            "numpadmultiply" => Ok(Code::NumpadMultiply),
            "numpaddivide" => Ok(Code::NumpadDivide),
            "numpaddecimal" => Ok(Code::NumpadDecimal),
            "numpadenter" => Ok(Code::NumpadEnter),

            "minus" | "-" => Ok(Code::Minus),
            "equal" | "=" => Ok(Code::Equal),
            "bracketleft" | "[" => Ok(Code::BracketLeft),
            "bracketright" | "]" => Ok(Code::BracketRight),
            "backslash" | "\\" => Ok(Code::Backslash),
            "semicolon" | ";" => Ok(Code::Semicolon),
            "quote" | "'" => Ok(Code::Quote),
            "backquote" | "`" => Ok(Code::Backquote),
            "comma" | "," => Ok(Code::Comma),
            "period" | "." => Ok(Code::Period),
            "slash" | "/" => Ok(Code::Slash),

            _ => Err(format!("Unknown key: {key}")),
        }
    }
}

impl Default for HotkeyManager {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for HotkeyManager {
    fn drop(&mut self) {
        self.unregister_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "global-hotkeys")]
    #[test]
    fn key_string_to_code_known_keys() {
        assert!(matches!(HotkeyManager::key_string_to_code("Space"), Ok(Code::Space)));
        assert!(matches!(HotkeyManager::key_string_to_code("space"), Ok(Code::Space)));
        assert!(matches!(HotkeyManager::key_string_to_code("F1"), Ok(Code::F1)));
        assert!(matches!(HotkeyManager::key_string_to_code("a"), Ok(Code::KeyA)));
        assert!(HotkeyManager::key_string_to_code("InvalidKey").is_err());
    }

    #[cfg(feature = "global-hotkeys")]
    #[test]
    fn modifiers_conversion() {
        let mods = HotkeyModifiers {
            ctrl: true,
            shift: true,
            alt: false,
            super_key: false,
        };
        let global = HotkeyManager::modifiers_to_global(&mods).unwrap();
        assert!(global.contains(Modifiers::CONTROL));
        assert!(global.contains(Modifiers::SHIFT));
        assert!(!global.contains(Modifiers::ALT));
    }

    #[cfg(feature = "global-hotkeys")]
    #[test]
    fn empty_modifiers() {
        let mods = HotkeyModifiers::default();
        assert!(HotkeyManager::modifiers_to_global(&mods).is_none());
    }

    #[test]
    fn hotkey_binding_display() {
        let binding = HotkeyBinding {
            modifiers: HotkeyModifiers {
                ctrl: true,
                shift: false,
                alt: false,
                super_key: false,
            },
            key: "Space".to_string(),
        };
        assert_eq!(binding.display(), "Ctrl+Space");

        let binding2 = HotkeyBinding {
            modifiers: HotkeyModifiers::default(),
            key: "F1".to_string(),
        };
        assert_eq!(binding2.display(), "F1");
    }

    #[test]
    fn legacy_keyboard_settings_migrate_into_shortcuts() {
        // Old JSON shape with the three single-binding fields.
        let json = serde_json::json!({
            "global_hotkeys_enabled": true,
            "ptt_hotkey": { "modifiers": { "ctrl": false, "shift": false, "alt": false, "super_key": false }, "key": "F1" },
            "toggle_mute_hotkey": { "modifiers": { "ctrl": true, "shift": false, "alt": false, "super_key": false }, "key": "M" },
        });
        let mut kb: KeyboardSettings = serde_json::from_value(json).unwrap();
        kb.normalize_legacy();

        assert_eq!(kb.shortcuts.len(), 2, "two legacy fields migrate to two entries");
        let ptt = kb
            .shortcuts
            .iter()
            .find(|s| s.function == HotkeyFunction::PushToTalk)
            .expect("PTT entry");
        assert_eq!(ptt.data, HotkeyData::Hold);
        assert_eq!(ptt.binding.as_ref().unwrap().key, "F1");

        let mute = kb
            .shortcuts
            .iter()
            .find(|s| s.function == HotkeyFunction::MuteSelf)
            .expect("Mute entry");
        assert_eq!(mute.data, HotkeyData::Toggle);
        assert!(mute.binding.as_ref().unwrap().modifiers.ctrl);

        // Round-trip: legacy fields are skip-serialized once drained.
        let reserialized = serde_json::to_value(&kb).unwrap();
        assert!(reserialized.get("ptt_hotkey").is_none());
        assert!(reserialized.get("shortcuts").is_some());
    }

    #[test]
    fn legacy_normalize_is_idempotent_on_repeat() {
        // First call migrates the legacy field; second call is a no-op
        // because the legacy slot is now None.
        let mut kb = KeyboardSettings {
            global_hotkeys_enabled: true,
            shortcuts: Vec::new(),
            legacy_ptt: Some(HotkeyBinding {
                modifiers: HotkeyModifiers::default(),
                key: "F2".to_string(),
            }),
            legacy_toggle_mute: None,
            legacy_toggle_deafen: None,
        };
        kb.normalize_legacy();
        assert_eq!(kb.shortcuts.len(), 1);
        assert!(kb.legacy_ptt.is_none(), "legacy field drained");
        kb.normalize_legacy();
        assert_eq!(kb.shortcuts.len(), 1, "second call is a no-op");
    }
}
