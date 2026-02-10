# Wayland Global Shortcuts via XDG Portal

**Status: IMPLEMENTED** (2026-01-28)

## Overview

Add support for global hotkeys on Wayland using the XDG Desktop Portal GlobalShortcuts interface. This replaces the current "no support" fallback with a proper portal-based solution that works on KDE Plasma, GNOME (47+), and Hyprland.

## Background

Wayland by design doesn't allow applications to grab global keyboard input directly (security feature). The standard solution is the `org.freedesktop.portal.GlobalShortcuts` D-Bus interface, which:

1. App creates a session and registers shortcuts with descriptions
2. Desktop environment shows a configuration dialog to the user
3. User binds actual keys via system settings
4. Portal sends `Activated`/`Deactivated` signals when shortcuts fire

## Current Architecture

```
hotkeys.rs:
├── HotkeyManager
│   ├── Uses global-hotkey crate (X11/Windows/macOS)
│   ├── Detects Wayland → returns no-op manager
│   └── Exposes poll_events() → Vec<HotkeyEvent>
│
app.rs:
├── Polls HotkeyManager every frame
├── Has fallback key detection via egui (window-focused)
└── Handles HotkeyEvent::{PttPressed, PttReleased, ToggleMute, ToggleDeafen}
```

## Proposed Architecture

```
hotkeys.rs:
├── HotkeyManager (unchanged API)
│   ├── Non-Wayland: uses global-hotkey crate (existing)
│   └── Wayland: uses PortalHotkeyBackend (new)
│
portal_hotkeys.rs (new):
├── PortalHotkeyBackend
│   ├── Connects to org.freedesktop.portal.GlobalShortcuts via ashpd
│   ├── Creates session on init
│   ├── Binds shortcuts with descriptions
│   ├── Spawns async task to listen for Activated/Deactivated signals
│   └── Sends events via channel to sync poll_events()
```

## Implementation Plan

### Phase 1: Add ashpd dependency and portal backend

1. **Add dependency** to `crates/egui-test/Cargo.toml`:
   ```toml
   ashpd = { version = "0.10", default-features = false, features = ["tokio"] }
   ```

2. **Create `portal_hotkeys.rs`** with:
   - `PortalHotkeyBackend` struct
   - Async initialization that creates GlobalShortcuts session
   - `bind_shortcuts()` method to register PTT, mute, deafen
   - Signal listener task that sends events via `tokio::sync::mpsc`
   - `poll_events()` that drains the channel (non-blocking)

3. **Wire into `HotkeyManager`**:
   - On Wayland, attempt portal connection
   - If portal unavailable, fall back to no-op (existing behavior)
   - `poll_events()` delegates to portal backend

### Phase 2: Session and shortcut management

4. **Session lifecycle**:
   - Create session on app startup
   - Bind shortcuts with user-friendly descriptions:
     - "push-to-talk": "Hold to transmit voice"
     - "toggle-mute": "Toggle microphone mute"
     - "toggle-deafen": "Toggle speaker mute"
   - Session persists for app lifetime

5. **Handle portal signals**:
   - `Activated` signal → map shortcut_id to HotkeyEvent
   - `Deactivated` signal → for PTT, emit PttReleased
   - Run listener in background tokio task

### Phase 3: User experience

6. **Settings UI updates**:
   - On Wayland with portal: show "Configure via System Settings" button
   - Button calls portal's `ConfigureShortcuts()` to open system dialog
   - Remove/update the orange "not supported" warning

7. **Graceful degradation**:
   - If portal not available (old DE, wlroots), keep window-focused fallback
   - Log appropriate messages for debugging

## API Design

```rust
// portal_hotkeys.rs

pub struct PortalHotkeyBackend {
    event_rx: tokio::sync::mpsc::UnboundedReceiver<HotkeyEvent>,
    // Handle kept alive to maintain session
    _session_handle: ashpd::desktop::global_shortcuts::GlobalShortcuts,
}

impl PortalHotkeyBackend {
    /// Attempt to connect to GlobalShortcuts portal.
    /// Returns None if portal unavailable.
    pub async fn new() -> Option<Self>;

    /// Bind the standard Rumble shortcuts.
    pub async fn bind_shortcuts(&self) -> Result<(), ashpd::Error>;

    /// Open system shortcut configuration dialog.
    pub async fn configure_shortcuts(&self) -> Result<(), ashpd::Error>;

    /// Poll for events (non-blocking, call from UI thread).
    pub fn poll_events(&mut self) -> Vec<HotkeyEvent>;
}
```

## Desktop Environment Support

| Environment | GlobalShortcuts Portal | Status |
|-------------|------------------------|--------|
| KDE Plasma 5.27+ | ✅ | Full support |
| GNOME 47+ | ✅ | Full support |
| Hyprland | ✅ | Full support |
| Sway/wlroots | ❌ | Falls back to window-focused |
| COSMIC | 🔄 | In development |

## Known Limitations

1. **No mouse button support** - Portal only handles keyboard shortcuts
2. **Modifier combinations** - Each modifier combo needs separate binding (F13, Shift+F13, etc.)
3. **User must configure keys** - App can only suggest, user sets actual binding
4. **No key capture in app** - Settings UI can't capture keys directly on Wayland

## Testing

1. **Unit tests**: Mock portal responses, verify event mapping
2. **Integration test**: Run on KDE/GNOME VM, verify shortcuts work
3. **Fallback test**: Verify wlroots systems still get window-focused PTT

## Files to Modify

- `crates/egui-test/Cargo.toml` - Add ashpd dependency
- `crates/egui-test/src/lib.rs` - Export new module
- `crates/egui-test/src/hotkeys.rs` - Integrate portal backend
- `crates/egui-test/src/portal_hotkeys.rs` - New file
- `crates/egui-test/src/app.rs` - Update settings UI for portal

## Implementation Notes (2026-01-28)

### What was implemented

1. **New file `portal_hotkeys.rs`**: Contains `PortalHotkeyBackend` that handles portal communication
2. **Modified `hotkeys.rs`**: Added `init_portal_backend()` async method and portal backend integration
3. **Modified `main.rs`**: Initializes portal backend in `EframeWrapper::new()`
4. **Modified `app.rs`**: Added `portal_hotkeys_available` field and updated settings UI

### How it works

1. On app startup, `HotkeyManager::new()` detects Wayland
2. `EframeWrapper::new()` calls `init_portal_backend()` which:
   - Connects to the GlobalShortcuts D-Bus portal via ashpd
   - Creates a session
   - Binds three shortcuts (push-to-talk, toggle-mute, toggle-deafen)
   - Spawns a background task to listen for Activated/Deactivated signals
3. `poll_events()` drains the event channel each frame
4. Settings UI shows portal status and registered shortcuts on Wayland

### Testing results

Tested on KDE Plasma (Wayland):
- Portal connects successfully
- Session created
- All 3 shortcuts bound
- Signal listener running

### Known issues

- Minor zbus thread panic during startup (non-fatal, portal still works)
- Users need to configure actual key bindings in system settings (by design)

## References

- [XDG GlobalShortcuts Portal Spec](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.GlobalShortcuts.html)
- [ashpd crate](https://docs.rs/ashpd/)
- [Mumble PR #5976](https://github.com/mumble-voip/mumble/pull/5976) - Reference implementation
