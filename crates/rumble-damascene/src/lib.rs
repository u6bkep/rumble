//! Rumble client built on the [damascene](https://github.com/computer-whisperer/damascene)
//! UI library.
//!
//! This is a fresh port of `rumble-egui` / `rumble-next` away from egui.
//! Damascene gives us proper SVG/icon support, color emoji, and a smaller
//! widget surface than egui.
//!
//! Architecture mirrors `rumble-next`: a [`UiBackend`] adapter wraps
//! `rumble_client::handle::BackendHandle` so the renderer reads `State`
//! snapshots and emits `Command`s. The damascene `App` impl owns local UI
//! state (form fields, selected room, expanded tree branches) and
//! reprojects `(state, ui_state)` into an `El` tree on every frame.

pub mod admin;
pub mod animated_gpu;
pub mod app;
pub mod backend;
pub mod chat;
pub mod elevate;
pub mod identity;
pub mod media_cache;
pub mod password_input;
pub mod room_acl;
pub mod room_tree;
pub mod server_picker;
pub mod settings;
pub mod theme;
pub mod video;
pub mod wizard;

pub use app::RumbleApp;
pub use backend::{NativeUiBackend, UiBackend};
pub use elevate::ElevateState;
pub use identity::Identity;
pub use server_picker::ServerForm;
pub use settings::{OpenSelect as SettingsOpenSelect, SettingsState, SettingsTab};
pub use wizard::{UnlockState, WizardState};
