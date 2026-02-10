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

    // RPC client mode: send command to running instance and exit
    if let Some(rpc_command) = &args.rpc {
        let socket_path = args
            .rpc_socket
            .as_ref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(backend::rpc::default_socket_path);

        let rt = tokio::runtime::Runtime::new().expect("Failed to create Tokio runtime for RPC");
        if let Err(e) = rt.block_on(egui_test::rpc_client::run_rpc_command(socket_path, rpc_command)) {
            eprintln!("RPC error: {}", e);
            std::process::exit(1);
        }
        std::process::exit(0);
    }

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
}
