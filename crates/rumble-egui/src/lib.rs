//! Rumble voice chat client library.
//!
//! This crate provides the core Rumble application logic and test harness.
//! The application can be run via:
//! - **eframe** (desktop): See `main.rs` for the native runner
//! - **test harness**: See [`TestHarness`] for automated testing with input injection

pub mod app;
pub mod first_run;
pub mod harness;
pub mod rpc_client;
pub mod settings;
pub mod toasts;

// Re-export the shared key-management module under its old name so
// existing call sites compile unchanged.
pub use rumble_desktop_shell::identity::key_manager;

/// The default backend handle, using the native platform implementations.
pub type BackendHandle = rumble_client::handle::BackendHandle<rumble_desktop::NativePlatform>;

pub use app::RumbleApp;
pub use harness::TestHarness;
pub use rumble_desktop_shell::{
    HotkeyBinding, HotkeyEvent, HotkeyManager, HotkeyModifiers, HotkeyRegistrationStatus, KeyboardSettings,
};
pub use settings::{Args, PersistentSettings};
pub use toasts::{Toast, ToastLevel, ToastManager};
