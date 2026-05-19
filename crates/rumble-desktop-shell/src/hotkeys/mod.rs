//! Global hotkey service shared between `rumble-egui` and `rumble-next`.
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
//!
//! Lifted from `rumble-egui/src/{hotkeys,portal_hotkeys}.rs`. The API
//! surface is intentionally identical so rumble-egui can re-export and
//! its existing call sites compile unchanged.

use std::collections::HashMap;

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

/// Persisted keyboard configuration. Lives in the hotkey module rather
/// than `settings::Settings` because the hotkey UI editor reads/writes
/// this directly and the types travel together.
///
/// Bindings are stored as a flat `Vec<ShortcutEntry>` so the user can
/// add multiple rows per function (multiple PTT keys, separate
/// MuteSelf+On / MuteSelf+Off rows). Each entry has a stable `id` used
/// both as the XDG portal shortcut ID (so the compositor preserves the
/// user's key assignment across re-binds) and as the UI row key.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// action (e.g. the deprecated rumble-egui keyboard panel).
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
        tracing::info!("Initializing XDG Portal GlobalShortcuts backend");
        match PortalHotkeyBackend::new(runtime_handle).await {
            Some(backend) => {
                tracing::info!("XDG Portal GlobalShortcuts session created (shortcuts bind lazily on settings load)");
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

        #[cfg(all(target_os = "linux", feature = "wayland-portal"))]
        if let Some(ref mut portal) = self.portal_backend {
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
            events.extend(portal.poll_events());
        }

        #[cfg(feature = "global-hotkeys")]
        if self.manager.is_some() {
            while let Ok(event) = GlobalHotKeyEvent::receiver().try_recv() {
                if let Some(&(function, data)) = self.hotkey_actions.get(&event.id) {
                    match event.state {
                        HotKeyState::Pressed => events.push(HotkeyEvent::Pressed { function, data }),
                        HotKeyState::Released => events.push(HotkeyEvent::Released { function, data }),
                    }
                }
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

    /// Convert egui Key to our string representation.
    #[cfg(feature = "egui-keys")]
    pub fn egui_key_to_string(key: eframe::egui::Key) -> Option<String> {
        use eframe::egui::Key;
        let s = match key {
            Key::Space => "Space",
            Key::Enter => "Enter",
            Key::Tab => "Tab",
            Key::Backspace => "Backspace",
            Key::Escape => "Escape",
            Key::Insert => "Insert",
            Key::Delete => "Delete",
            Key::Home => "Home",
            Key::End => "End",
            Key::PageUp => "PageUp",
            Key::PageDown => "PageDown",
            Key::ArrowUp => "ArrowUp",
            Key::ArrowDown => "ArrowDown",
            Key::ArrowLeft => "ArrowLeft",
            Key::ArrowRight => "ArrowRight",
            Key::F1 => "F1",
            Key::F2 => "F2",
            Key::F3 => "F3",
            Key::F4 => "F4",
            Key::F5 => "F5",
            Key::F6 => "F6",
            Key::F7 => "F7",
            Key::F8 => "F8",
            Key::F9 => "F9",
            Key::F10 => "F10",
            Key::F11 => "F11",
            Key::F12 => "F12",
            Key::A => "A",
            Key::B => "B",
            Key::C => "C",
            Key::D => "D",
            Key::E => "E",
            Key::F => "F",
            Key::G => "G",
            Key::H => "H",
            Key::I => "I",
            Key::J => "J",
            Key::K => "K",
            Key::L => "L",
            Key::M => "M",
            Key::N => "N",
            Key::O => "O",
            Key::P => "P",
            Key::Q => "Q",
            Key::R => "R",
            Key::S => "S",
            Key::T => "T",
            Key::U => "U",
            Key::V => "V",
            Key::W => "W",
            Key::X => "X",
            Key::Y => "Y",
            Key::Z => "Z",
            Key::Num0 => "0",
            Key::Num1 => "1",
            Key::Num2 => "2",
            Key::Num3 => "3",
            Key::Num4 => "4",
            Key::Num5 => "5",
            Key::Num6 => "6",
            Key::Num7 => "7",
            Key::Num8 => "8",
            Key::Num9 => "9",
            Key::Minus => "Minus",
            Key::Equals => "Equal",
            Key::OpenBracket => "BracketLeft",
            Key::CloseBracket => "BracketRight",
            Key::Backslash => "Backslash",
            Key::Semicolon => "Semicolon",
            Key::Quote => "Quote",
            Key::Backtick => "Backquote",
            Key::Comma => "Comma",
            Key::Period => "Period",
            Key::Slash => "Slash",
            _ => return None,
        };
        Some(s.to_string())
    }

    /// Convert key string to egui Key (for window-focused fallback).
    #[cfg(feature = "egui-keys")]
    pub fn key_string_to_egui_key(key: &str) -> Option<eframe::egui::Key> {
        use eframe::egui::Key;
        match key.to_lowercase().as_str() {
            "space" => Some(Key::Space),
            "enter" | "return" => Some(Key::Enter),
            "tab" => Some(Key::Tab),
            "backspace" => Some(Key::Backspace),
            "escape" | "esc" => Some(Key::Escape),
            "insert" => Some(Key::Insert),
            "delete" => Some(Key::Delete),
            "home" => Some(Key::Home),
            "end" => Some(Key::End),
            "pageup" => Some(Key::PageUp),
            "pagedown" => Some(Key::PageDown),
            "arrowup" | "up" => Some(Key::ArrowUp),
            "arrowdown" | "down" => Some(Key::ArrowDown),
            "arrowleft" | "left" => Some(Key::ArrowLeft),
            "arrowright" | "right" => Some(Key::ArrowRight),
            "f1" => Some(Key::F1),
            "f2" => Some(Key::F2),
            "f3" => Some(Key::F3),
            "f4" => Some(Key::F4),
            "f5" => Some(Key::F5),
            "f6" => Some(Key::F6),
            "f7" => Some(Key::F7),
            "f8" => Some(Key::F8),
            "f9" => Some(Key::F9),
            "f10" => Some(Key::F10),
            "f11" => Some(Key::F11),
            "f12" => Some(Key::F12),
            "a" | "keya" => Some(Key::A),
            "b" | "keyb" => Some(Key::B),
            "c" | "keyc" => Some(Key::C),
            "d" | "keyd" => Some(Key::D),
            "e" | "keye" => Some(Key::E),
            "f" | "keyf" => Some(Key::F),
            "g" | "keyg" => Some(Key::G),
            "h" | "keyh" => Some(Key::H),
            "i" | "keyi" => Some(Key::I),
            "j" | "keyj" => Some(Key::J),
            "k" | "keyk" => Some(Key::K),
            "l" | "keyl" => Some(Key::L),
            "m" | "keym" => Some(Key::M),
            "n" | "keyn" => Some(Key::N),
            "o" | "keyo" => Some(Key::O),
            "p" | "keyp" => Some(Key::P),
            "q" | "keyq" => Some(Key::Q),
            "r" | "keyr" => Some(Key::R),
            "s" | "keys" => Some(Key::S),
            "t" | "keyt" => Some(Key::T),
            "u" | "keyu" => Some(Key::U),
            "v" | "keyv" => Some(Key::V),
            "w" | "keyw" => Some(Key::W),
            "x" | "keyx" => Some(Key::X),
            "y" | "keyy" => Some(Key::Y),
            "z" | "keyz" => Some(Key::Z),
            "0" | "digit0" => Some(Key::Num0),
            "1" | "digit1" => Some(Key::Num1),
            "2" | "digit2" => Some(Key::Num2),
            "3" | "digit3" => Some(Key::Num3),
            "4" | "digit4" => Some(Key::Num4),
            "5" | "digit5" => Some(Key::Num5),
            "6" | "digit6" => Some(Key::Num6),
            "7" | "digit7" => Some(Key::Num7),
            "8" | "digit8" => Some(Key::Num8),
            "9" | "digit9" => Some(Key::Num9),
            "minus" | "-" => Some(Key::Minus),
            "equal" | "=" => Some(Key::Equals),
            "bracketleft" | "[" => Some(Key::OpenBracket),
            "bracketright" | "]" => Some(Key::CloseBracket),
            "backslash" | "\\" => Some(Key::Backslash),
            "semicolon" | ";" => Some(Key::Semicolon),
            "quote" | "'" => Some(Key::Quote),
            "backquote" | "`" => Some(Key::Backtick),
            "comma" | "," => Some(Key::Comma),
            "period" | "." => Some(Key::Period),
            "slash" | "/" => Some(Key::Slash),
            _ => None,
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

    #[cfg(feature = "egui-keys")]
    #[test]
    fn key_string_to_egui_roundtrip() {
        use eframe::egui::Key;
        for key in [Key::Space, Key::F1, Key::A, Key::Num0, Key::Enter] {
            let s = HotkeyManager::egui_key_to_string(key).unwrap();
            assert_eq!(HotkeyManager::key_string_to_egui_key(&s), Some(key));
        }
    }
}
