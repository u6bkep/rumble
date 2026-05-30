//! Password-masked variant of damascene's `text_input`.
//!
//! Thin shim over [`damascene_core::widgets::text_input::text_input_with`]
//! with [`MaskMode::Password`] — damascene handles the bullet display and
//! the global selection routing, so we just provide the right key and
//! masked options.

use damascene_core::prelude::*;

/// Render a password field with `value` masked as bullets.
pub fn password_input(value: &str, selection: &Selection, key: &str) -> El {
    text_input_with(value, selection, key, TextInputOpts::default().password())
}

/// Apply a routed `UiEvent` to a password field. Mirrors
/// `text_input::apply_event` but sets the password mask so paste / max-
/// length behave consistently with the rendered field.
pub fn apply_event(value: &mut String, selection: &mut Selection, key: &str, event: &UiEvent) -> bool {
    text_input::apply_event_with(value, selection, key, event, &TextInputOpts::default().password())
}
