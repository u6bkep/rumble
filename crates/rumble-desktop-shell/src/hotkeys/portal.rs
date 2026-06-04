//! XDG Desktop Portal GlobalShortcuts backend for Wayland.
//!
//! Provides global hotkey support on Wayland via the
//! `org.freedesktop.portal.GlobalShortcuts` D-Bus interface.
//!
//! Supported environments: KDE Plasma 5.27+, GNOME 47+, Hyprland.
//! Older Wayland compositors fail gracefully via `PortalHotkeyBackend::new
//! → None` and the manager falls back to window-focused shortcuts.
//!
//! ## Dynamic shortcut binding
//!
//! Shortcuts are not declared once at startup — the user edits the
//! Shortcuts settings table, which calls `set_shortcuts` with the
//! new entry list. We then call `BindShortcuts` again on the existing
//! session; the compositor preserves the user's key assignment against
//! stable `ShortcutEntry::id` UUIDs across calls. New rows show up in
//! the system shortcut settings UI tagged with their portal
//! description (`HotkeyFunction::label() + HotkeyData::label()`).

use std::{collections::HashMap, sync::Arc};

use ashpd::desktop::{
    Session,
    global_shortcuts::{GlobalShortcuts, NewShortcut},
};
use futures_util::StreamExt;
use tokio::sync::{RwLock, mpsc};

use super::{HotkeyData, HotkeyEvent, HotkeyFunction, ShortcutEntry};

#[derive(Debug, Clone)]
pub struct ShortcutInfo {
    pub id: String,
    pub description: String,
    /// Empty until the user binds something in the system settings.
    pub trigger_description: String,
}

/// Shared state between the foreground API and the background listener
/// task. Updated by `set_shortcuts` (which rewrites both maps) and by
/// the listener's `ShortcutsChanged` handler (which refreshes
/// `triggers`).
#[derive(Default)]
struct PortalState {
    /// Map from portal shortcut id -> (function, data) so the listener
    /// can post the right `HotkeyEvent` when a shortcut fires.
    actions: HashMap<String, (HotkeyFunction, HotkeyData)>,
    /// Map from portal shortcut id -> trigger description string. Kept
    /// in sync with `BindShortcuts` responses and `ShortcutsChanged`
    /// signals so the UI shows what the user actually bound.
    triggers: HashMap<String, String>,
}

pub struct PortalHotkeyBackend {
    event_rx: mpsc::UnboundedReceiver<HotkeyEvent>,
    /// Kept around so the listener task's sender stays alive even when
    /// the UI hasn't drained recently.
    _event_tx: mpsc::UnboundedSender<HotkeyEvent>,
    shortcuts: Arc<GlobalShortcuts>,
    session: Arc<Session<GlobalShortcuts>>,
    state: Arc<RwLock<PortalState>>,
    runtime_handle: tokio::runtime::Handle,
    /// Handle to the background signal-listener task. Aborted and
    /// awaited in [`Self::shutdown`] so the task releases its
    /// `Arc<GlobalShortcuts>` (and the D-Bus signal streams it holds)
    /// before we drop our own zbus references.
    listener_handle: tokio::task::JoinHandle<()>,
    /// True once we've successfully bound at least one shortcut. Drives
    /// `is_available` for the manager-side fallback decision.
    has_bound: bool,
}

impl PortalHotkeyBackend {
    /// Connect to the portal, create a session, and spawn the signal
    /// listener. The session is empty initially — call `set_shortcuts`
    /// once the user's settings are loaded to actually bind anything.
    /// Returns `None` if portal connection / session creation fails.
    pub async fn new(runtime_handle: tokio::runtime::Handle) -> Option<Self> {
        tracing::info!("Attempting to connect to XDG GlobalShortcuts portal");

        let shortcuts = match GlobalShortcuts::new().await {
            Ok(s) => Arc::new(s),
            Err(e) => {
                tracing::warn!("GlobalShortcuts portal not available: {e}");
                return None;
            }
        };

        let session = match shortcuts.create_session(Default::default()).await {
            Ok(s) => Arc::new(s),
            Err(e) => {
                tracing::warn!("Failed to create GlobalShortcuts session: {e}");
                return None;
            }
        };

        let state = Arc::new(RwLock::new(PortalState::default()));
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        let listener_shortcuts = Arc::clone(&shortcuts);
        let listener_state = Arc::clone(&state);
        let listener_tx = event_tx.clone();
        let listener_handle = runtime_handle.spawn(async move {
            Self::listen_for_signals(listener_shortcuts, listener_state, listener_tx).await;
        });

        Some(Self {
            event_rx,
            _event_tx: event_tx,
            shortcuts,
            session,
            state,
            runtime_handle,
            listener_handle,
            has_bound: false,
        })
    }

    /// Re-bind the portal session against the user's current shortcut
    /// list. Stable entry IDs mean the compositor preserves whatever
    /// key the user has assigned per row, even when rows are added or
    /// reordered.
    ///
    /// Called from the synchronous settings path; dispatches the actual
    /// `BindShortcuts` D-Bus call to the runtime, since `bind_shortcuts`
    /// is async and we don't want the UI thread to wait. The state map
    /// is updated synchronously so subsequent `poll_events` can dispatch
    /// the new (function, data) mapping immediately.
    pub fn set_shortcuts(&mut self, entries: &[ShortcutEntry]) {
        let definitions: Vec<NewShortcut> = entries
            .iter()
            .map(|e| NewShortcut::new(&e.id, e.portal_description()))
            .collect();
        let new_actions: HashMap<String, (HotkeyFunction, HotkeyData)> =
            entries.iter().map(|e| (e.id.clone(), (e.function, e.data))).collect();

        self.has_bound = !definitions.is_empty();

        let shortcuts = Arc::clone(&self.shortcuts);
        let session = Arc::clone(&self.session);
        let state = Arc::clone(&self.state);
        self.runtime_handle.spawn(async move {
            {
                let mut guard = state.write().await;
                guard.actions = new_actions;
                // Preserve existing trigger strings for rows that still
                // exist; drop entries for removed rows. The compositor
                // will re-emit `ShortcutsChanged` shortly with the new
                // authoritative set.
                let live_ids: std::collections::HashSet<String> = guard.actions.keys().cloned().collect();
                guard.triggers.retain(|id, _| live_ids.contains(id));
            }

            if definitions.is_empty() {
                tracing::debug!("set_shortcuts: empty entry list, skipping BindShortcuts");
                return;
            }

            match shortcuts
                .bind_shortcuts(&session, &definitions, None, Default::default())
                .await
            {
                Ok(request) => match request.response() {
                    Ok(response) => {
                        let bound = response.shortcuts();
                        tracing::info!("BindShortcuts: {} entries bound", bound.len());
                        let mut guard = state.write().await;
                        for s in bound {
                            guard
                                .triggers
                                .insert(s.id().to_string(), s.trigger_description().to_string());
                        }
                    }
                    // `Cancelled` is what most compositors return when the
                    // user dismisses the "this app wants to register new
                    // global shortcuts" prompt. Not fatal — the user can
                    // retry by clicking a Shortcut cell, which routes
                    // back through `open_settings` → BindShortcuts.
                    Err(e) => {
                        tracing::info!("BindShortcuts not applied ({e}); reopen the Shortcuts settings cell to retry")
                    }
                },
                Err(e) => tracing::warn!("BindShortcuts call failed: {e}"),
            }
        });
    }

    /// Listen for Activated / Deactivated / ShortcutsChanged signals
    /// and translate them into `HotkeyEvent`s for the UI thread.
    async fn listen_for_signals(
        shortcuts: Arc<GlobalShortcuts>,
        state: Arc<RwLock<PortalState>>,
        event_tx: mpsc::UnboundedSender<HotkeyEvent>,
    ) {
        tracing::debug!("Starting GlobalShortcuts signal listener");

        let mut activated = match shortcuts.receive_activated().await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("Failed to subscribe to Activated signals: {e}");
                return;
            }
        };

        let mut deactivated = match shortcuts.receive_deactivated().await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("Failed to subscribe to Deactivated signals: {e}");
                return;
            }
        };

        // `ShortcutsChanged` is non-fatal — log on failure, then the
        // listener falls back to leaving triggers cached from the last
        // `BindShortcuts` response.
        let changed = match shortcuts.receive_shortcuts_changed().await {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!("Failed to subscribe to ShortcutsChanged signals: {e}");
                None
            }
        };

        match changed {
            Some(mut changed) => loop {
                tokio::select! {
                    Some(ev) = activated.next() => {
                        if Self::handle_activated(&state, &event_tx, ev).await.is_break() {
                            break;
                        }
                    }
                    Some(ev) = deactivated.next() => {
                        if Self::handle_deactivated(&state, &event_tx, ev).await.is_break() {
                            break;
                        }
                    }
                    Some(ev) = changed.next() => {
                        let mut guard = state.write().await;
                        guard.triggers.clear();
                        for s in ev.shortcuts() {
                            guard.triggers.insert(s.id().to_string(), s.trigger_description().to_string());
                        }
                        tracing::debug!("ShortcutsChanged: {} entries", guard.triggers.len());
                    }
                    else => break,
                }
            },
            None => loop {
                tokio::select! {
                    Some(ev) = activated.next() => {
                        if Self::handle_activated(&state, &event_tx, ev).await.is_break() {
                            break;
                        }
                    }
                    Some(ev) = deactivated.next() => {
                        if Self::handle_deactivated(&state, &event_tx, ev).await.is_break() {
                            break;
                        }
                    }
                    else => break,
                }
            },
        }
        tracing::debug!("Signal streams ended");
    }

    async fn handle_activated(
        state: &Arc<RwLock<PortalState>>,
        event_tx: &mpsc::UnboundedSender<HotkeyEvent>,
        ev: ashpd::desktop::global_shortcuts::Activated,
    ) -> std::ops::ControlFlow<()> {
        let id = ev.shortcut_id().to_string();
        let action = state.read().await.actions.get(&id).copied();
        if let Some((function, data)) = action
            && event_tx.send(HotkeyEvent::Pressed { function, data }).is_err()
        {
            return std::ops::ControlFlow::Break(());
        }
        std::ops::ControlFlow::Continue(())
    }

    async fn handle_deactivated(
        state: &Arc<RwLock<PortalState>>,
        event_tx: &mpsc::UnboundedSender<HotkeyEvent>,
        ev: ashpd::desktop::global_shortcuts::Deactivated,
    ) -> std::ops::ControlFlow<()> {
        let id = ev.shortcut_id().to_string();
        let action = state.read().await.actions.get(&id).copied();
        if let Some((function, data)) = action
            && event_tx.send(HotkeyEvent::Released { function, data }).is_err()
        {
            return std::ops::ControlFlow::Break(());
        }
        std::ops::ControlFlow::Continue(())
    }

    pub fn is_available(&self) -> bool {
        self.has_bound
    }

    pub fn poll_events(&mut self) -> Vec<HotkeyEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.event_rx.try_recv() {
            events.push(event);
        }
        events
    }

    /// Trigger description string the compositor reported for this
    /// entry id. Returns `None` if the entry isn't bound or the user
    /// hasn't assigned a key yet (compositor reports empty string).
    pub fn trigger_for(&self, entry_id: &str) -> Option<String> {
        let guard = self.state.try_read().ok()?;
        guard.triggers.get(entry_id).filter(|s| !s.is_empty()).cloned()
    }

    /// Snapshot of all currently-bound shortcuts. Used by the rumble-
    /// egui settings panel that still expects the historical
    /// `ShortcutInfo` shape.
    pub fn get_shortcuts(&self) -> Vec<ShortcutInfo> {
        let guard = match self.state.try_read() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        guard
            .actions
            .iter()
            .map(|(id, (function, data))| {
                let description = format!("{} ({})", function.label(), data.label());
                let trigger_description = guard.triggers.get(id).cloned().unwrap_or_default();
                ShortcutInfo {
                    id: id.clone(),
                    description,
                    trigger_description,
                }
            })
            .collect()
    }

    /// Open the compositor's shortcut configuration dialog.
    ///
    /// On portals exposing v2 of the GlobalShortcuts interface this calls
    /// `ConfigureShortcuts`, which is the proper "show the configuration
    /// UI again" request. On v1-only portals (Plasma 5.27 / older Plasma
    /// 6) there's no such method — `BindShortcuts` deliberately *does
    /// not* re-show the dialog once shortcuts are bound, per the portal
    /// spec — so we shell out to the desktop environment's own shortcut
    /// settings application as a best-effort fallback. New shortcut rows
    /// added from the Shortcuts tab still trigger their own
    /// `BindShortcuts` prompt via `set_shortcuts`, so this path only
    /// matters for *re-configuring* already-bound rows.
    pub fn open_settings(&self) {
        let shortcuts = Arc::clone(&self.shortcuts);
        let session = Arc::clone(&self.session);
        self.runtime_handle.spawn(async move {
            match shortcuts.configure_shortcuts(&session, None, Default::default()).await {
                Ok(()) => return,
                Err(e) => {
                    tracing::info!("ConfigureShortcuts unavailable ({e}); launching DE shortcut settings");
                }
            }
            spawn_system_shortcut_settings();
        });
    }

    /// Tear down the portal session and its D-Bus connection cleanly.
    ///
    /// Consumes `self`, so the `Arc<GlobalShortcuts>` / `Arc<Session>`
    /// references it owns are dropped at the end of this future. Must be
    /// driven from inside a tokio runtime context (e.g. via
    /// `runtime.block_on`): the underlying zbus connection spawns onto
    /// the tokio executor as it closes, and panics with "there is no
    /// reactor running" if dropped with no runtime in scope.
    ///
    /// We abort the listener task and await it first so it releases its
    /// own `Arc<GlobalShortcuts>` clone (and the signal streams it holds)
    /// *before* our references drop — that makes this `shutdown` the last
    /// holder, so the connection actually closes here, inside the runtime,
    /// rather than later on the main thread during `App` teardown.
    pub async fn shutdown(self) {
        self.listener_handle.abort();
        // The abort propagates as a JoinError(cancelled); awaiting it
        // guarantees the task's future has been dropped before we fall
        // through and drop our own Arcs at end of scope.
        let _ = self.listener_handle.await;
    }
}

/// Best-effort launch of the desktop environment's keyboard-shortcut
/// settings page. Used as the v1-portal fallback for re-opening shortcut
/// configuration — the portal spec doesn't give us a programmatic way to
/// re-show the dialog there, so we hand control over to the DE.
///
/// Spawned children are detached: closing damascene doesn't take them down.
/// `XDG_CURRENT_DESKTOP` is colon-separated on some setups; lowercase
/// substring match is sufficient.
fn spawn_system_shortcut_settings() {
    let de = std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .to_ascii_lowercase();

    // Candidate commands in priority order. `kcm_keys` is the shortcuts
    // module Plasma 5 and 6 both ship; on Plasma 6 `kcmshell6` is the
    // launcher, on Plasma 5 it's `kcmshell5`. `systemsettings` opens
    // the same module inside the full Settings app. We deliberately
    // don't list speculative kcm names — `Command::spawn` returns Ok
    // even when the kcm module doesn't exist, so a wrong guess would
    // flash an error window without falling through to the next entry.
    let candidates: &[(&str, &[&str])] = if de.contains("kde") || de.contains("plasma") {
        &[
            ("kcmshell6", &["kcm_keys"]),
            ("kcmshell5", &["kcm_keys"]),
            ("systemsettings", &["kcm_keys"]),
        ]
    } else if de.contains("gnome") {
        &[("gnome-control-center", &["keyboard"])]
    } else if de.contains("cinnamon") {
        &[("cinnamon-settings", &["keyboard"])]
    } else if de.contains("xfce") {
        &[("xfce4-keyboard-settings", &[])]
    } else {
        &[]
    };

    for (cmd, args) in candidates {
        match std::process::Command::new(cmd).args(*args).spawn() {
            Ok(_) => {
                tracing::info!("Launched {cmd} for system shortcut settings");
                return;
            }
            Err(e) => tracing::debug!("Could not launch {cmd}: {e}"),
        }
    }
    tracing::warn!(
        "No known shortcut-settings application found for XDG_CURRENT_DESKTOP={:?}; please open your desktop's \
         keyboard shortcuts manually.",
        std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default(),
    );
}
