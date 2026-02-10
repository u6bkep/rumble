//! Rumble voice chat client - eframe runner.
//!
//! This is the native desktop runner for the Rumble application.
//! It uses eframe to create a window and run the egui-based UI.

use clap::Parser;
use eframe::egui;
use egui_test::{Args, HotkeyManager, PersistentSettings, RumbleApp};

fn main() -> eframe::Result<()> {
    // Build the default env filter, then append a directive to suppress the
    // clipboard "supported mime-type is not found" error that fires on
    // Wayland/X11 when the clipboard contains image data instead of text.
    // The egui_winit clipboard code logs this at ERROR via the `log` crate
    // (bridged to tracing), but it is expected when pasting images.
    let env_filter =
        tracing_subscriber::EnvFilter::from_default_env().add_directive("egui_winit::clipboard=off".parse().unwrap());

    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    let args = Args::parse();

    // Load settings to get keyboard config
    let settings = PersistentSettings::load();

    // Create hotkey manager on main thread (required for macOS).
    // This must be done before eframe::run_native which takes over the main thread.
    let mut hotkey_manager = HotkeyManager::new();
    if let Err(e) = hotkey_manager.register_from_settings(&settings.keyboard) {
        tracing::error!("Failed to register global hotkeys: {}", e);
    }

    // Create the tokio runtime - this will be passed to RumbleApp
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("Failed to create Tokio runtime");

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1000.0, 700.0])
            .with_min_inner_size([800.0, 500.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Rumble",
        options,
        Box::new(move |cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);
            Ok(Box::new(EframeWrapper::new(
                cc.egui_ctx.clone(),
                runtime,
                args,
                hotkey_manager,
            )))
        }),
    )
}

/// Wrapper that implements `eframe::App` for `RumbleApp`.
///
/// This keeps the `RumbleApp` independent of eframe, allowing it to be
/// used with other runners like the test harness.
struct EframeWrapper {
    app: RumbleApp,
    /// Global hotkey manager for PTT and other shortcuts.
    hotkey_manager: HotkeyManager,
    /// Keep the runtime alive for the lifetime of the application.
    _runtime: tokio::runtime::Runtime,
    /// Clipboard for reading image data (via arboard, independent of egui/smithay).
    clipboard: Option<arboard::Clipboard>,
    /// State machine for detecting Ctrl+V image paste.
    /// When Ctrl/Cmd is first pressed, we start counting frames. If no text
    /// `Paste` event appears within the window, we check arboard for image
    /// data and handle it.
    paste_detect: PasteDetectState,
}

/// State machine for detecting clipboard image paste attempts.
///
/// egui_winit intercepts Ctrl+V and tries to read text from the clipboard.
/// On Wayland (smithay) and X11 (arboard), if the clipboard only contains
/// image data, the text read fails and the key event is silently consumed --
/// no `Event::Paste` or `Event::Key { key: V }` ever reaches the app.
///
/// To work around this, we watch for the Cmd/Ctrl modifier becoming active
/// and then wait a few frames. If a text `Paste` event arrives, everything
/// is fine (text paste). If no `Paste` arrives and no other command-chord
/// event (Copy, Cut, or a Key with the command modifier) is seen, we check
/// arboard for image data and handle it as an image paste.
#[derive(Default)]
struct PasteDetectState {
    /// Whether Cmd/Ctrl was held in the previous frame.
    prev_command_held: bool,
    /// Remaining frames to wait after Cmd/Ctrl was first pressed.
    /// 0 means not waiting.
    frames_remaining: u8,
    /// Whether we saw a text paste or other command-chord event during the
    /// wait window (meaning the Ctrl press was NOT a failed image paste).
    saw_command_action: bool,
}

impl PasteDetectState {
    /// Number of frames to wait after Ctrl is pressed before concluding that
    /// a paste attempt failed. 3 frames (~50ms at 60fps) is long enough for
    /// the V key event to arrive in the same or next frame.
    const WAIT_FRAMES: u8 = 3;
}

impl EframeWrapper {
    fn new(
        ctx: egui::Context,
        runtime: tokio::runtime::Runtime,
        args: Args,
        mut hotkey_manager: HotkeyManager,
    ) -> Self {
        // Initialize XDG Portal backend for Wayland global shortcuts
        let handle = runtime.handle().clone();
        runtime.block_on(async {
            hotkey_manager.init_portal_backend(handle).await;
        });

        let mut app = RumbleApp::new(ctx, runtime.handle().clone(), args);

        // Tell the app if portal hotkeys are available (for settings UI)
        app.set_portal_hotkeys_available(hotkey_manager.has_portal_backend());

        // Pass registration status to the app for UI indicators
        app.set_hotkey_registration_status(hotkey_manager.registration_status_map().clone());

        // Pass portal shortcuts and callback to the app (Linux/Wayland only)
        #[cfg(target_os = "linux")]
        {
            app.set_portal_shortcuts(hotkey_manager.get_portal_shortcuts());
        }

        Self {
            app,
            hotkey_manager,
            _runtime: runtime,
            clipboard: arboard::Clipboard::new().ok(),
            paste_detect: PasteDetectState::default(),
        }
    }

    /// Update portal shortcuts from the hotkey manager.
    #[cfg(target_os = "linux")]
    fn update_portal_shortcuts(&mut self) {
        self.app
            .set_portal_shortcuts(self.hotkey_manager.get_portal_shortcuts());
    }
}

impl eframe::App for EframeWrapper {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Poll global hotkey events first (works even when window unfocused)
        for event in self.hotkey_manager.poll_events() {
            self.app.handle_hotkey_event(event);
        }

        self.app.render(ctx);
    }

    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        let command_held = raw_input.modifiers.command;
        let pd = &mut self.paste_detect;

        // Check if any events indicate a successful command action (text
        // paste, copy, cut, or any key press with the command modifier).
        let has_command_action = raw_input.events.iter().any(|e| {
            matches!(
                e,
                egui::Event::Paste(_)
                    | egui::Event::Copy
                    | egui::Event::Cut
                    | egui::Event::Key {
                        pressed: true,
                        modifiers: egui::Modifiers { command: true, .. },
                        ..
                    }
            )
        });

        if has_command_action {
            pd.saw_command_action = true;
        }

        // Detect Cmd/Ctrl rising edge -> start the wait window.
        if command_held && !pd.prev_command_held {
            pd.frames_remaining = PasteDetectState::WAIT_FRAMES;
            pd.saw_command_action = has_command_action;
        }

        // Count down the wait window.
        if pd.frames_remaining > 0 {
            pd.frames_remaining -= 1;

            // When the window expires, check if we should handle image paste.
            if pd.frames_remaining == 0 && !pd.saw_command_action {
                if let Some(clipboard) = &mut self.clipboard {
                    if let Ok(img_data) = clipboard.get_image() {
                        self.app.handle_clipboard_image_paste(img_data);
                    }
                }
            }
        }

        // If Cmd/Ctrl was released, reset state.
        if !command_held {
            pd.frames_remaining = 0;
            pd.saw_command_action = false;
        }

        pd.prev_command_held = command_held;
    }
}
