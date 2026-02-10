//! Rumble voice chat client library.
//!
//! This crate provides the core Rumble application logic and test harness.
//! The application can be run via:
//! - **eframe** (desktop): See `main.rs` for the native runner
//! - **test harness**: See [`TestHarness`] for automated testing with input injection

pub mod app;
pub mod harness;
pub mod hotkeys;
pub mod key_manager;
#[cfg(target_os = "linux")]
pub mod portal_hotkeys;
pub mod rpc_client;
pub mod settings;
pub mod toasts;

pub use app::RumbleApp;
pub use harness::TestHarness;
pub use hotkeys::{HotkeyAction, HotkeyEvent, HotkeyManager, HotkeyRegistrationStatus};
pub use settings::{Args, HotkeyBinding, HotkeyModifiers, KeyboardSettings, PersistentSettings};
pub use toasts::ToastManager;
