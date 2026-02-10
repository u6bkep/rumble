//! XDG Desktop Portal GlobalShortcuts backend for Wayland.
//!
//! This module provides global hotkey support on Wayland via the
//! `org.freedesktop.portal.GlobalShortcuts` D-Bus interface.
//!
//! **Supported Desktop Environments:**
//! - KDE Plasma 5.27+
//! - GNOME 47+
//! - Hyprland
//!
//! On unsupported environments (Sway, older GNOME, etc.), initialization
//! will fail gracefully and the app falls back to window-focused shortcuts.

use crate::hotkeys::HotkeyEvent;
use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};
use futures_util::StreamExt;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{RwLock, mpsc};

/// Shortcut IDs used for portal registration.
pub const SHORTCUT_PTT: &str = "push-to-talk";
pub const SHORTCUT_MUTE: &str = "toggle-mute";
pub const SHORTCUT_DEAFEN: &str = "toggle-deafen";

/// Information about a bound shortcut.
#[derive(Debug, Clone)]
pub struct ShortcutInfo {
    /// The shortcut ID (e.g., "push-to-talk")
    pub id: String,
    /// Human-readable description of the shortcut action
    pub description: String,
    /// The key binding configured by the user (e.g., "Super+P", "F13")
    /// May be empty if the user hasn't configured a key yet.
    pub trigger_description: String,
}

/// Shared state that can be accessed from the UI thread.
#[derive(Default)]
pub struct PortalShortcutState {
    /// Currently bound shortcuts with their trigger descriptions.
    pub shortcuts: Vec<ShortcutInfo>,
}

/// Portal-based global shortcuts backend for Wayland.
///
/// This backend connects to the XDG Desktop Portal GlobalShortcuts interface
/// and translates portal signals into `HotkeyEvent`s that the rest of the
/// app can consume.
pub struct PortalHotkeyBackend {
    /// Channel receiver for hotkey events from the portal listener task.
    event_rx: mpsc::UnboundedReceiver<HotkeyEvent>,
    /// Sender kept for potential future use (e.g., shutdown signal).
    _event_tx: mpsc::UnboundedSender<HotkeyEvent>,
    /// Whether shortcuts have been bound successfully.
    shortcuts_bound: bool,
    /// Shared state accessible from UI thread.
    state: Arc<RwLock<PortalShortcutState>>,
    /// Tokio runtime handle for async operations.
    runtime_handle: tokio::runtime::Handle,
}

impl PortalHotkeyBackend {
    /// Attempt to create a new portal hotkey backend.
    ///
    /// This will:
    /// 1. Connect to the GlobalShortcuts portal
    /// 2. Create a session
    /// 3. Bind the standard Rumble shortcuts
    /// 4. Start listening for activation signals
    ///
    /// Returns `None` if the portal is unavailable or initialization fails.
    pub async fn new(runtime_handle: tokio::runtime::Handle) -> Option<Self> {
        tracing::info!("Attempting to connect to XDG GlobalShortcuts portal");

        // Try to create the GlobalShortcuts proxy
        let shortcuts = match GlobalShortcuts::new().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("GlobalShortcuts portal not available: {}", e);
                return None;
            }
        };

        tracing::info!("Connected to GlobalShortcuts portal");

        // Create session
        let session = match shortcuts.create_session().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("Failed to create GlobalShortcuts session: {}", e);
                return None;
            }
        };

        tracing::info!("Created GlobalShortcuts session");

        // Define our shortcuts with human-readable descriptions
        let shortcut_definitions = shortcut_definitions();

        // Shared state for UI access
        let state = Arc::new(RwLock::new(PortalShortcutState::default()));

        // Bind shortcuts - this may show a system dialog
        let shortcuts_bound = match shortcuts.bind_shortcuts(&session, &shortcut_definitions, None).await {
            Ok(request) => {
                // Wait for the response
                match request.response() {
                    Ok(response) => {
                        let bound = response.shortcuts();
                        tracing::info!("Shortcuts bound successfully: {} shortcuts", bound.len());

                        // Store the bound shortcuts in shared state
                        let mut state_guard = state.write().await;
                        state_guard.shortcuts = bound
                            .iter()
                            .map(|s| {
                                tracing::info!("Shortcut '{}' bound: {}", s.id(), s.trigger_description());
                                ShortcutInfo {
                                    id: s.id().to_string(),
                                    description: shortcut_description(s.id()),
                                    trigger_description: s.trigger_description().to_string(),
                                }
                            })
                            .collect();
                        drop(state_guard);

                        !bound.is_empty()
                    }
                    Err(e) => {
                        tracing::warn!("Failed to get bind_shortcuts response: {}", e);
                        false
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Failed to bind shortcuts: {}", e);
                false
            }
        };

        // Create event channel
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        // Spawn the signal listener task
        let event_tx_clone = event_tx.clone();
        runtime_handle.spawn(async move {
            Self::listen_for_signals(shortcuts, event_tx_clone).await;
        });

        Some(Self {
            event_rx,
            _event_tx: event_tx,
            shortcuts_bound,
            state,
            runtime_handle,
        })
    }

    /// Listen for Activated/Deactivated signals from the portal.
    async fn listen_for_signals(shortcuts: GlobalShortcuts<'static>, event_tx: mpsc::UnboundedSender<HotkeyEvent>) {
        tracing::debug!("Starting GlobalShortcuts signal listener");

        // Map shortcut IDs to event handlers
        let shortcut_map: HashMap<&str, fn(bool) -> Option<HotkeyEvent>> = [
            (
                SHORTCUT_PTT,
                (|pressed| {
                    Some(if pressed {
                        HotkeyEvent::PttPressed
                    } else {
                        HotkeyEvent::PttReleased
                    })
                }) as fn(bool) -> Option<HotkeyEvent>,
            ),
            (
                SHORTCUT_MUTE,
                (|pressed| {
                    if pressed { Some(HotkeyEvent::ToggleMute) } else { None }
                }) as fn(bool) -> Option<HotkeyEvent>,
            ),
            (
                SHORTCUT_DEAFEN,
                (|pressed| {
                    if pressed { Some(HotkeyEvent::ToggleDeafen) } else { None }
                }) as fn(bool) -> Option<HotkeyEvent>,
            ),
        ]
        .into_iter()
        .collect();

        // Listen for activated signals
        let mut activated_stream = match shortcuts.receive_activated().await {
            Ok(stream) => stream,
            Err(e) => {
                tracing::error!("Failed to subscribe to Activated signals: {}", e);
                return;
            }
        };

        // Listen for deactivated signals
        let mut deactivated_stream = match shortcuts.receive_deactivated().await {
            Ok(stream) => stream,
            Err(e) => {
                tracing::error!("Failed to subscribe to Deactivated signals: {}", e);
                return;
            }
        };

        loop {
            tokio::select! {
                Some(activated) = activated_stream.next() => {
                    let shortcut_id = activated.shortcut_id();
                    tracing::debug!("Shortcut activated: {}", shortcut_id);

                    if let Some(handler) = shortcut_map.get(shortcut_id) {
                        if let Some(event) = handler(true) {
                            if event_tx.send(event).is_err() {
                                tracing::debug!("Event channel closed, stopping listener");
                                break;
                            }
                        }
                    }
                }
                Some(deactivated) = deactivated_stream.next() => {
                    let shortcut_id = deactivated.shortcut_id();
                    tracing::debug!("Shortcut deactivated: {}", shortcut_id);

                    if let Some(handler) = shortcut_map.get(shortcut_id) {
                        if let Some(event) = handler(false) {
                            if event_tx.send(event).is_err() {
                                tracing::debug!("Event channel closed, stopping listener");
                                break;
                            }
                        }
                    }
                }
                else => {
                    tracing::debug!("Signal streams ended");
                    break;
                }
            }
        }
    }

    /// Check if shortcuts were successfully bound.
    pub fn is_available(&self) -> bool {
        self.shortcuts_bound
    }

    /// Poll for hotkey events (non-blocking).
    ///
    /// This should be called from the UI thread each frame.
    pub fn poll_events(&mut self) -> Vec<HotkeyEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.event_rx.try_recv() {
            events.push(event);
        }
        events
    }

    /// Get the current shortcut bindings.
    ///
    /// This returns a snapshot of the shortcuts as they were last bound.
    /// Call `refresh_shortcuts` to update after system settings change.
    pub fn get_shortcuts(&self) -> Vec<ShortcutInfo> {
        // Use try_read to avoid blocking the UI thread
        self.state
            .try_read()
            .map(|guard| guard.shortcuts.clone())
            .unwrap_or_default()
    }

    /// Open the system shortcut configuration dialog.
    ///
    /// This triggers a new bind_shortcuts call which should open the
    /// system's shortcut configuration UI (on KDE, GNOME, etc.).
    pub fn open_settings(&self) {
        let state = self.state.clone();
        self.runtime_handle.spawn(async move {
            if let Err(e) = open_shortcut_settings_and_update(state).await {
                tracing::error!("Failed to open shortcut settings: {}", e);
            }
        });
    }
}

/// Get the shortcut definitions with their descriptions.
fn shortcut_definitions() -> Vec<NewShortcut> {
    vec![
        NewShortcut::new(SHORTCUT_PTT, "Hold to transmit voice (Push-to-Talk)"),
        NewShortcut::new(SHORTCUT_MUTE, "Toggle microphone mute"),
        NewShortcut::new(SHORTCUT_DEAFEN, "Toggle speaker mute (deafen)"),
    ]
}

/// Get a human-readable description for a shortcut ID.
fn shortcut_description(id: &str) -> String {
    match id {
        SHORTCUT_PTT => "Push-to-Talk".to_string(),
        SHORTCUT_MUTE => "Toggle Mute".to_string(),
        SHORTCUT_DEAFEN => "Toggle Deafen".to_string(),
        _ => id.to_string(),
    }
}

/// Open the system shortcut configuration dialog and update the state.
///
/// This creates a new session and triggers the bind dialog, which allows
/// the user to configure shortcuts in system settings. The updated bindings
/// are stored in the provided state.
async fn open_shortcut_settings_and_update(state: Arc<RwLock<PortalShortcutState>>) -> Result<(), ashpd::Error> {
    let shortcuts = GlobalShortcuts::new().await?;
    let session = shortcuts.create_session().await?;

    // Re-bind shortcuts to trigger the config dialog
    let definitions = shortcut_definitions();

    let request = shortcuts.bind_shortcuts(&session, &definitions, None).await?;

    // Wait for the response and update state with new bindings
    match request.response() {
        Ok(response) => {
            let bound = response.shortcuts();
            tracing::info!("Shortcuts reconfigured: {} shortcuts bound", bound.len());

            let mut state_guard = state.write().await;
            state_guard.shortcuts = bound
                .iter()
                .map(|s| {
                    tracing::info!("Shortcut '{}' now bound to: {}", s.id(), s.trigger_description());
                    ShortcutInfo {
                        id: s.id().to_string(),
                        description: shortcut_description(s.id()),
                        trigger_description: s.trigger_description().to_string(),
                    }
                })
                .collect();
        }
        Err(e) => {
            tracing::warn!("Failed to get bind_shortcuts response: {}", e);
        }
    }

    Ok(())
}

/// Open the system shortcut configuration dialog (standalone function).
///
/// This is a convenience function that doesn't update any state.
/// Prefer using `PortalHotkeyBackend::open_settings()` if you have access
/// to the backend instance.
pub async fn open_shortcut_settings() -> Result<(), ashpd::Error> {
    let shortcuts = GlobalShortcuts::new().await?;
    let session = shortcuts.create_session().await?;

    let definitions = shortcut_definitions();

    shortcuts.bind_shortcuts(&session, &definitions, None).await?;

    Ok(())
}
