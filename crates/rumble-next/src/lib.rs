//! Rumble-next — parallel GUI built from the claude-design/Rumble
//! paradigm wireframes, rendered with `rumble-widgets` themes and
//! wired to the live `rumble-client` backend.

pub mod adapters;
pub mod app;
pub mod backend;
pub mod connect_view;
pub mod data;
#[cfg(feature = "test-harness")]
pub mod harness;
pub mod identity;
pub mod paradigm;
pub mod settings_panel;
pub mod shell;
pub mod toasts;

pub use app::{App, Paradigm};
#[cfg(feature = "test-harness")]
pub use backend::MockBackend;
pub use backend::{NativeUiBackend, UiBackend};
pub use toasts::{Toast, ToastLevel, ToastManager};

#[cfg(feature = "test-harness")]
pub use harness::TestHarness;
