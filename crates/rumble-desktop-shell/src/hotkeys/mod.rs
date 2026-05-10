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

/// Hotkey action types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HotkeyAction {
    PushToTalk,
    ToggleMute,
    ToggleDeafen,
}

/// Registration status for a hotkey action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyRegistrationStatus {
    /// Hotkey registered successfully.
    Registered,
    /// Registration failed.
    Failed,
    /// No binding configured for this action.
    NotConfigured,
}

/// Result of polling the manager. Drained by the UI thread each frame.
#[derive(Debug, Clone, Copy)]
pub enum HotkeyEvent {
    PttPressed,
    PttReleased,
    ToggleMute,
    ToggleDeafen,
}

/// Manages global hotkey registration and events.
///
/// On macOS this MUST be created on the main thread (a `global-hotkey`
/// requirement that bubbles up to us).
pub struct HotkeyManager {
    #[cfg(feature = "global-hotkeys")]
    manager: Option<GlobalHotKeyManager>,
    /// Map from registered hotkey id to the action that fired it.
    #[cfg(feature = "global-hotkeys")]
    hotkey_actions: HashMap<u32, HotkeyAction>,
    /// Registered hotkeys (held so we can unregister cleanly on drop).
    #[cfg(feature = "global-hotkeys")]
    registered_hotkeys: Vec<HotKey>,
    /// Current status per action; drives the keyboard settings UI.
    registration_status: HashMap<HotkeyAction, HotkeyRegistrationStatus>,
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
                if backend.is_available() {
                    tracing::info!("XDG Portal GlobalShortcuts backend initialized successfully");
                } else {
                    tracing::warn!("XDG Portal connected but no shortcuts bound — user may need to configure them");
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

    pub fn get_registration_status(&self, action: HotkeyAction) -> HotkeyRegistrationStatus {
        self.registration_status
            .get(&action)
            .copied()
            .unwrap_or(HotkeyRegistrationStatus::NotConfigured)
    }

    pub fn registration_status_map(&self) -> &HashMap<HotkeyAction, HotkeyRegistrationStatus> {
        &self.registration_status
    }

    /// Reconcile registered hotkeys with `settings`. Always re-registers
    /// from scratch — diffing is fiddly and the operation is rare (only
    /// when the user changes a binding).
    pub fn register_from_settings(&mut self, settings: &KeyboardSettings) -> Result<(), String> {
        self.unregister_all();

        // Seed every action as NotConfigured so the UI can render a
        // stable status grid even when bindings are missing.
        self.registration_status
            .insert(HotkeyAction::PushToTalk, HotkeyRegistrationStatus::NotConfigured);
        self.registration_status
            .insert(HotkeyAction::ToggleMute, HotkeyRegistrationStatus::NotConfigured);
        self.registration_status
            .insert(HotkeyAction::ToggleDeafen, HotkeyRegistrationStatus::NotConfigured);

        if !settings.global_hotkeys_enabled {
            tracing::info!("Global hotkeys disabled in settings");
            return Ok(());
        }

        #[cfg(feature = "global-hotkeys")]
        {
            let Some(ref manager) = self.manager else {
                return Ok(());
            };

            for (action, binding) in [
                (HotkeyAction::PushToTalk, &settings.ptt_hotkey),
                (HotkeyAction::ToggleMute, &settings.toggle_mute_hotkey),
                (HotkeyAction::ToggleDeafen, &settings.toggle_deafen_hotkey),
            ] {
                let Some(binding) = binding else { continue };
                match Self::binding_to_hotkey(binding) {
                    Ok(hotkey) => {
                        let id = hotkey.id();
                        if let Err(e) = manager.register(hotkey) {
                            tracing::warn!("Failed to register {action:?}: {e}");
                            self.registration_status
                                .insert(action, HotkeyRegistrationStatus::Failed);
                        } else {
                            self.hotkey_actions.insert(id, action);
                            self.registered_hotkeys.push(hotkey);
                            self.registration_status
                                .insert(action, HotkeyRegistrationStatus::Registered);
                            tracing::info!("Registered {action:?}: {}", binding.display());
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Invalid {action:?} binding: {e}");
                        self.registration_status
                            .insert(action, HotkeyRegistrationStatus::Failed);
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
                if let Some(&action) = self.hotkey_actions.get(&event.id) {
                    match action {
                        HotkeyAction::PushToTalk => match event.state {
                            HotKeyState::Pressed => events.push(HotkeyEvent::PttPressed),
                            HotKeyState::Released => events.push(HotkeyEvent::PttReleased),
                        },
                        HotkeyAction::ToggleMute => {
                            if event.state == HotKeyState::Pressed {
                                events.push(HotkeyEvent::ToggleMute);
                            }
                        }
                        HotkeyAction::ToggleDeafen => {
                            if event.state == HotKeyState::Pressed {
                                events.push(HotkeyEvent::ToggleDeafen);
                            }
                        }
                    }
                }
            }
        }

        events
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
    fn key_string_to_egui_roundtrip() {
        use eframe::egui::Key;
        for key in [Key::Space, Key::F1, Key::A, Key::Num0, Key::Enter] {
            let s = HotkeyManager::egui_key_to_string(key).unwrap();
            assert_eq!(HotkeyManager::key_string_to_egui_key(&s), Some(key));
        }
    }
}
