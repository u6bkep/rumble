//! Sudo / superuser elevation prompt.
//!
//! Asks for the server-side sudo password and dispatches
//! `Command::Elevate { password }`. The server replies with a
//! `CommandResult` that the client surfaces as a local chat message
//! (`✔ Elevated to superuser` / `✖ Incorrect password`), and on
//! success broadcasts `UserStatusChanged { is_elevated: true }` —
//! which flips the local user's name to gold in the room tree.

use damascene_core::prelude::*;

use crate::password_input;

#[derive(Debug, Default)]
pub struct ElevateState {
    pub password: String,
    pub error: Option<String>,
}

pub enum ElevateOutcome {
    /// Event wasn't for the elevate modal.
    Ignored,
    /// Event was consumed; no further action.
    Handled,
    /// User pressed Cancel / Escape / scrim. Close the modal.
    Cancel,
    /// User submitted a non-empty password. Caller should fire
    /// `Command::Elevate { password }` and close the modal.
    Submit { password: String },
}

const KEY: &str = "elevate";
const KEY_DISMISS: &str = "elevate:dismiss";
const KEY_PWD: &str = "elevate:pwd";
const KEY_CANCEL: &str = "elevate:cancel";
const KEY_SUBMIT: &str = "elevate:submit";

pub fn render(state: &ElevateState, selection: &Selection) -> El {
    let mut body: Vec<El> = vec![
        paragraph(
            "Enter the server's sudo password to elevate this session to superuser. Elevation bypasses the ACL system \
             until you disconnect.",
        )
        .muted(),
        password_input::password_input(&state.password, selection, KEY_PWD),
    ];
    if let Some(err) = &state.error {
        body.push(form_message(err.clone()));
    }
    let submit = if state.password.is_empty() {
        button("Elevate").key(KEY_SUBMIT).disabled()
    } else {
        button("Elevate").key(KEY_SUBMIT).primary()
    };
    body.push(
        row([button("Cancel").key(KEY_CANCEL), spacer(), submit])
            .gap(tokens::SPACE_2)
            .width(Size::Fill(1.0))
            .align(Align::Center),
    );
    modal(KEY, "Elevate to superuser", body)
}

pub fn handle_event(state: &mut ElevateState, event: &UiEvent, selection: &mut Selection) -> ElevateOutcome {
    if event.is_click_or_activate(KEY_DISMISS) || event.is_click_or_activate(KEY_CANCEL) {
        return ElevateOutcome::Cancel;
    }
    if let UiEventKind::KeyDown = event.kind
        && let Some(kp) = event.key_press.as_ref()
        && matches!(kp.key, UiKey::Escape)
    {
        return ElevateOutcome::Cancel;
    }
    if event.target_key() == Some(KEY_PWD) {
        // Submit on bare Enter — matches the unlock prompt's UX.
        if let UiEventKind::KeyDown = event.kind
            && let Some(kp) = event.key_press.as_ref()
            && matches!(kp.key, UiKey::Enter)
            && !kp.modifiers.shift
            && !state.password.is_empty()
        {
            return ElevateOutcome::Submit {
                password: state.password.clone(),
            };
        }
        password_input::apply_event(&mut state.password, selection, KEY_PWD, event);
        return ElevateOutcome::Handled;
    }
    if event.is_click_or_activate(KEY_SUBMIT) && !state.password.is_empty() {
        return ElevateOutcome::Submit {
            password: state.password.clone(),
        };
    }
    ElevateOutcome::Ignored
}
