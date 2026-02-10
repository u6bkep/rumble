//! Global hotkey management for Rumble.
//!
//! This module provides cross-platform global hotkey support (hotkeys that work
//! even when the application window is not focused). Essential for push-to-talk
//! while gaming or using other applications.
//!
//! **Platform Support:**
//! - Windows: Full support
//! - macOS: Full support (requires main thread)
//! - Linux X11: Full support
//! - Linux Wayland: XDG Portal GlobalShortcuts (KDE Plasma, GNOME 47+, Hyprland)

use crate::settings::{HotkeyBinding, HotkeyModifiers, KeyboardSettings};
use global_hotkey::{
    GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState,
    hotkey::{Code, HotKey, Modifiers},
};
use std::collections::HashMap;

#[cfg(target_os = "linux")]
use crate::portal_hotkeys::PortalHotkeyBackend;

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

/// Result of checking for hotkey events.
#[derive(Debug, Clone, Copy)]
pub enum HotkeyEvent {
    /// PTT key was pressed.
    PttPressed,
    /// PTT key was released.
    PttReleased,
    /// Toggle mute was pressed.
    ToggleMute,
    /// Toggle deafen was pressed.
    ToggleDeafen,
}

/// Manages global hotkey registration and events.
///
/// **Important:** On macOS, this MUST be created on the main thread.
pub struct HotkeyManager {
    manager: Option<GlobalHotKeyManager>,
    /// Map from hotkey ID to action.
    hotkey_actions: HashMap<u32, HotkeyAction>,
    /// Registered hotkeys (for unregistration).
    registered_hotkeys: Vec<HotKey>,
    /// Registration status per action (populated by register_from_settings).
    registration_status: HashMap<HotkeyAction, HotkeyRegistrationStatus>,
    /// Whether we're running on Wayland.
    is_wayland: bool,
    /// Whether initialization failed.
    init_failed: bool,
    /// XDG Portal backend for Wayland (Linux only).
    #[cfg(target_os = "linux")]
    portal_backend: Option<PortalHotkeyBackend>,
}

impl HotkeyManager {
    /// Create a new HotkeyManager.
    ///
    /// Returns a manager that may or may not have global hotkey support,
    /// depending on the platform and environment.
    ///
    /// On Wayland, call [`init_portal_backend`] after the tokio runtime is available
    /// to enable XDG Portal-based global shortcuts.
    pub fn new() -> Self {
        let is_wayland = Self::detect_wayland();

        if is_wayland {
            tracing::info!("Running on Wayland - will use XDG Portal for global shortcuts if available");
            return Self {
                manager: None,
                hotkey_actions: HashMap::new(),
                registered_hotkeys: Vec::new(),
                registration_status: HashMap::new(),
                is_wayland: true,
                init_failed: false,
                #[cfg(target_os = "linux")]
                portal_backend: None,
            };
        }

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
                    #[cfg(target_os = "linux")]
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
                    #[cfg(target_os = "linux")]
                    portal_backend: None,
                }
            }
        }
    }

    /// Initialize the XDG Portal backend for Wayland global shortcuts.
    ///
    /// This should be called after the tokio runtime is available. On non-Wayland
    /// systems or if the portal is unavailable, this is a no-op.
    #[cfg(target_os = "linux")]
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
                    tracing::warn!("XDG Portal connected but no shortcuts bound - user may need to configure them");
                }
                self.portal_backend = Some(backend);
            }
            None => {
                tracing::warn!("XDG Portal GlobalShortcuts not available - falling back to window-focused shortcuts");
            }
        }
    }

    /// Initialize the XDG Portal backend (no-op on non-Linux).
    #[cfg(not(target_os = "linux"))]
    pub async fn init_portal_backend(&mut self, _runtime_handle: tokio::runtime::Handle) {
        // No-op on non-Linux platforms
    }

    /// Check if portal-based shortcuts are available (Wayland).
    #[cfg(target_os = "linux")]
    pub fn has_portal_backend(&self) -> bool {
        self.portal_backend.as_ref().map(|b| b.is_available()).unwrap_or(false)
    }

    /// Check if portal-based shortcuts are available (always false on non-Linux).
    #[cfg(not(target_os = "linux"))]
    pub fn has_portal_backend(&self) -> bool {
        false
    }

    /// Get the current portal shortcut bindings (Linux/Wayland only).
    #[cfg(target_os = "linux")]
    pub fn get_portal_shortcuts(&self) -> Vec<crate::portal_hotkeys::ShortcutInfo> {
        self.portal_backend
            .as_ref()
            .map(|b| b.get_shortcuts())
            .unwrap_or_default()
    }

    /// Get the current portal shortcut bindings (always empty on non-Linux).
    #[cfg(not(target_os = "linux"))]
    pub fn get_portal_shortcuts(&self) -> Vec<crate::portal_hotkeys::ShortcutInfo> {
        Vec::new()
    }

    /// Open the system shortcut configuration dialog (Linux/Wayland only).
    #[cfg(target_os = "linux")]
    pub fn open_portal_settings(&self) {
        if let Some(ref backend) = self.portal_backend {
            backend.open_settings();
        }
    }

    /// Open the system shortcut configuration dialog (no-op on non-Linux).
    #[cfg(not(target_os = "linux"))]
    pub fn open_portal_settings(&self) {
        // No-op on non-Linux
    }

    /// Detect if running on Wayland.
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

    /// Check if global hotkeys are available.
    pub fn is_available(&self) -> bool {
        self.manager.is_some()
    }

    /// Check if running on Wayland.
    pub fn is_wayland(&self) -> bool {
        self.is_wayland
    }

    /// Check if initialization failed (not due to Wayland).
    pub fn init_failed(&self) -> bool {
        self.init_failed
    }

    /// Get the registration status for a given hotkey action.
    pub fn get_registration_status(&self, action: HotkeyAction) -> HotkeyRegistrationStatus {
        self.registration_status
            .get(&action)
            .copied()
            .unwrap_or(HotkeyRegistrationStatus::NotConfigured)
    }

    /// Get the full registration status map.
    pub fn registration_status_map(&self) -> &HashMap<HotkeyAction, HotkeyRegistrationStatus> {
        &self.registration_status
    }

    /// Register hotkeys from settings.
    pub fn register_from_settings(&mut self, settings: &KeyboardSettings) -> Result<(), String> {
        // Unregister existing hotkeys first
        self.unregister_all();

        // Initialize all actions as not-configured
        self.registration_status
            .insert(HotkeyAction::PushToTalk, HotkeyRegistrationStatus::NotConfigured);
        self.registration_status
            .insert(HotkeyAction::ToggleMute, HotkeyRegistrationStatus::NotConfigured);
        self.registration_status
            .insert(HotkeyAction::ToggleDeafen, HotkeyRegistrationStatus::NotConfigured);

        let Some(ref manager) = self.manager else {
            return Ok(()); // No manager = no-op
        };

        if !settings.global_hotkeys_enabled {
            tracing::info!("Global hotkeys disabled in settings");
            return Ok(());
        }

        // Register PTT hotkey
        if let Some(ref binding) = settings.ptt_hotkey {
            match Self::binding_to_hotkey(binding) {
                Ok(hotkey) => {
                    let id = hotkey.id();
                    if let Err(e) = manager.register(hotkey.clone()) {
                        tracing::warn!("Failed to register PTT hotkey: {}", e);
                        self.registration_status
                            .insert(HotkeyAction::PushToTalk, HotkeyRegistrationStatus::Failed);
                    } else {
                        self.hotkey_actions.insert(id, HotkeyAction::PushToTalk);
                        self.registered_hotkeys.push(hotkey);
                        self.registration_status
                            .insert(HotkeyAction::PushToTalk, HotkeyRegistrationStatus::Registered);
                        tracing::info!("Registered PTT hotkey: {}", binding.display());
                    }
                }
                Err(e) => {
                    tracing::warn!("Invalid PTT hotkey binding: {}", e);
                    self.registration_status
                        .insert(HotkeyAction::PushToTalk, HotkeyRegistrationStatus::Failed);
                }
            }
        }

        // Register toggle mute hotkey
        if let Some(ref binding) = settings.toggle_mute_hotkey {
            match Self::binding_to_hotkey(binding) {
                Ok(hotkey) => {
                    let id = hotkey.id();
                    if let Err(e) = manager.register(hotkey.clone()) {
                        tracing::warn!("Failed to register mute hotkey: {}", e);
                        self.registration_status
                            .insert(HotkeyAction::ToggleMute, HotkeyRegistrationStatus::Failed);
                    } else {
                        self.hotkey_actions.insert(id, HotkeyAction::ToggleMute);
                        self.registered_hotkeys.push(hotkey);
                        self.registration_status
                            .insert(HotkeyAction::ToggleMute, HotkeyRegistrationStatus::Registered);
                        tracing::info!("Registered mute hotkey: {}", binding.display());
                    }
                }
                Err(e) => {
                    tracing::warn!("Invalid mute hotkey binding: {}", e);
                    self.registration_status
                        .insert(HotkeyAction::ToggleMute, HotkeyRegistrationStatus::Failed);
                }
            }
        }

        // Register toggle deafen hotkey
        if let Some(ref binding) = settings.toggle_deafen_hotkey {
            match Self::binding_to_hotkey(binding) {
                Ok(hotkey) => {
                    let id = hotkey.id();
                    if let Err(e) = manager.register(hotkey.clone()) {
                        tracing::warn!("Failed to register deafen hotkey: {}", e);
                        self.registration_status
                            .insert(HotkeyAction::ToggleDeafen, HotkeyRegistrationStatus::Failed);
                    } else {
                        self.hotkey_actions.insert(id, HotkeyAction::ToggleDeafen);
                        self.registered_hotkeys.push(hotkey);
                        self.registration_status
                            .insert(HotkeyAction::ToggleDeafen, HotkeyRegistrationStatus::Registered);
                        tracing::info!("Registered deafen hotkey: {}", binding.display());
                    }
                }
                Err(e) => {
                    tracing::warn!("Invalid deafen hotkey binding: {}", e);
                    self.registration_status
                        .insert(HotkeyAction::ToggleDeafen, HotkeyRegistrationStatus::Failed);
                }
            }
        }

        Ok(())
    }

    /// Unregister all hotkeys.
    pub fn unregister_all(&mut self) {
        if let Some(ref manager) = self.manager {
            for hotkey in &self.registered_hotkeys {
                let _ = manager.unregister(hotkey.clone());
            }
        }
        self.hotkey_actions.clear();
        self.registered_hotkeys.clear();
    }

    /// Poll for hotkey events (non-blocking).
    pub fn poll_events(&mut self) -> Vec<HotkeyEvent> {
        let mut events = Vec::new();

        // Poll XDG Portal backend on Linux/Wayland
        #[cfg(target_os = "linux")]
        if let Some(ref mut portal) = self.portal_backend {
            events.extend(portal.poll_events());
        }

        // Poll global-hotkey backend (X11, Windows, macOS)
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

    /// Convert a HotkeyBinding to a global_hotkey::HotKey.
    fn binding_to_hotkey(binding: &HotkeyBinding) -> Result<HotKey, String> {
        let modifiers = Self::modifiers_to_global(&binding.modifiers);
        let code = Self::key_string_to_code(&binding.key)?;
        Ok(HotKey::new(modifiers, code))
    }

    /// Convert our modifiers to global_hotkey modifiers.
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

    /// Convert key string to Code enum.
    pub fn key_string_to_code(key: &str) -> Result<Code, String> {
        // Map common key names to Code variants
        match key.to_lowercase().as_str() {
            // Special keys
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

            // Arrow keys
            "arrowup" | "up" => Ok(Code::ArrowUp),
            "arrowdown" | "down" => Ok(Code::ArrowDown),
            "arrowleft" | "left" => Ok(Code::ArrowLeft),
            "arrowright" | "right" => Ok(Code::ArrowRight),

            // Function keys
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

            // Letters
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

            // Numbers
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

            // Numpad
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

            // Punctuation and symbols
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

            _ => Err(format!("Unknown key: {}", key)),
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

    #[test]
    fn test_key_string_to_code() {
        assert!(matches!(HotkeyManager::key_string_to_code("Space"), Ok(Code::Space)));
        assert!(matches!(HotkeyManager::key_string_to_code("space"), Ok(Code::Space)));
        assert!(matches!(HotkeyManager::key_string_to_code("F1"), Ok(Code::F1)));
        assert!(matches!(HotkeyManager::key_string_to_code("a"), Ok(Code::KeyA)));
        assert!(HotkeyManager::key_string_to_code("InvalidKey").is_err());
    }

    #[test]
    fn test_modifiers_conversion() {
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

    #[test]
    fn test_empty_modifiers() {
        let mods = HotkeyModifiers::default();
        assert!(HotkeyManager::modifiers_to_global(&mods).is_none());
    }

    #[test]
    fn test_hotkey_binding_display() {
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
    fn test_key_string_to_egui_key() {
        use eframe::egui::Key;

        assert_eq!(HotkeyManager::key_string_to_egui_key("Space"), Some(Key::Space));
        assert_eq!(HotkeyManager::key_string_to_egui_key("space"), Some(Key::Space));
        assert_eq!(HotkeyManager::key_string_to_egui_key("F1"), Some(Key::F1));
        assert_eq!(HotkeyManager::key_string_to_egui_key("A"), Some(Key::A));
        assert_eq!(HotkeyManager::key_string_to_egui_key("a"), Some(Key::A));
        assert_eq!(HotkeyManager::key_string_to_egui_key("0"), Some(Key::Num0));
        assert_eq!(HotkeyManager::key_string_to_egui_key("InvalidKey"), None);
    }
}
