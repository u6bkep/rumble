//! First-run identity wizard + encrypted-key unlock prompt.
//!
//! Ports the flow from `rumble-egui::first_run` / `rumble-next::app`
//! onto aetna primitives so a brand-new user can generate or pick an
//! identity from this client without falling back to one of the egui
//! clients first.

use rumble_desktop_shell::KeyInfo;
use tokio::task::JoinHandle;

use crate::{identity::Identity, password_input};
use aetna_core::prelude::*;

#[derive(Debug, Clone, Default)]
pub enum WizardState {
    /// Key already configured. Wizard is hidden.
    #[default]
    NotNeeded,
    /// First-run picker: local key vs ssh-agent.
    SelectMethod,
    /// Local-key generator with optional password.
    GenerateLocal {
        password: String,
        confirm: String,
        error: Option<String>,
    },
    /// Awaiting `connect_and_list_keys` to return.
    ConnectingAgent,
    /// Picker over the agent's Ed25519 keys.
    SelectAgentKey {
        keys: Vec<KeyInfo>,
        selected: Option<usize>,
        error: Option<String>,
    },
    /// Generate-and-add-to-agent comment input.
    GenerateAgentKey { comment: String },
    /// Terminal error screen; user can rewind to `SelectMethod`.
    Error { message: String },
    /// Wizard finished successfully — caller should clear.
    Complete,
}

#[derive(Default)]
pub struct UnlockState {
    pub password: String,
    pub error: Option<String>,
}

/// In-flight async ssh-agent op. We block_on the handle once
/// `is_finished()` flips true so the result lands on the same frame.
pub enum PendingAgentOp {
    Connect(JoinHandle<anyhow::Result<Vec<KeyInfo>>>),
    AddKey(JoinHandle<anyhow::Result<KeyInfo>>),
}

/// Outcome of a single event for the wizard flow. Returned to the App
/// so it can poll async ops, perform side effects on the runtime, etc.
pub enum WizardOutcome {
    /// Event was not for the wizard.
    Ignored,
    /// Event was consumed; no further work for the App.
    Handled,
    /// Spawn `connect_and_list_keys` in the runtime; result feeds back
    /// into `WizardState::SelectAgentKey`.
    SpawnConnect,
    /// Spawn `generate_and_add_to_agent(comment)`; result feeds back
    /// into `WizardState::Complete`.
    SpawnAddKey { comment: String },
    /// Apply this password through `Identity::generate_local_key`.
    GenerateLocal { password: Option<String> },
    /// Save this agent key via `Identity::select_agent_key`.
    SelectAgentKey { key_info: KeyInfo },
    /// Try `Identity::unlock(...)` with this password.
    Unlock { password: String },
}

// ============================================================
// Render
// ============================================================

const KEY_GEN_LOCAL: &str = "wizard:gen-local";
const KEY_USE_AGENT: &str = "wizard:use-agent";
const KEY_BACK: &str = "wizard:back";
const KEY_SUBMIT: &str = "wizard:submit";
const KEY_CANCEL_AGENT: &str = "wizard:cancel-agent";
const KEY_GEN_AGENT_KEY: &str = "wizard:gen-agent-key";
const KEY_PWD: &str = "wizard:pwd";
const KEY_CONFIRM: &str = "wizard:confirm";
const KEY_AGENT_COMMENT: &str = "wizard:comment";
const KEY_UNLOCK_PWD: &str = "unlock:pwd";
const KEY_UNLOCK_SUBMIT: &str = "unlock:submit";

pub fn agent_row_key(idx: usize) -> String {
    format!("wizard:agent-row:{idx}")
}

pub fn parse_agent_row_key(key: &str) -> Option<usize> {
    key.strip_prefix("wizard:agent-row:").and_then(|s| s.parse().ok())
}

pub fn render(state: &WizardState, agent_busy: bool, selection: &Selection) -> Option<El> {
    match state {
        WizardState::NotNeeded | WizardState::Complete => None,
        WizardState::SelectMethod => Some(render_select_method()),
        WizardState::GenerateLocal {
            password,
            confirm,
            error,
        } => Some(render_generate_local(password, confirm, error.as_deref(), selection)),
        WizardState::ConnectingAgent => Some(render_connecting_agent()),
        WizardState::SelectAgentKey { keys, selected, error } => {
            Some(render_select_agent_key(keys, *selected, error.as_deref()))
        }
        WizardState::GenerateAgentKey { comment } => Some(render_generate_agent_key(comment, agent_busy, selection)),
        WizardState::Error { message } => Some(render_error(message)),
    }
}

fn render_select_method() -> El {
    let agent_available = ssh_agent_available();
    let agent_button = if agent_available {
        button("Choose SSH agent key").key(KEY_USE_AGENT).primary()
    } else {
        button("Choose SSH agent key").key(KEY_USE_AGENT).disabled()
    };

    let mut body: Vec<El> = vec![
        paragraph("Rumble uses an Ed25519 key as your server identity. Pick where this client should keep that key."),
        divider(),
        text("Generate a local key").semibold(),
        paragraph("Store a new key on this computer, optionally password-protected.").muted(),
        row([button("Generate local key").key(KEY_GEN_LOCAL).primary()]).align(Align::Start),
        divider(),
        text("Use an SSH agent key").semibold(),
        paragraph("Use an Ed25519 key already loaded in ssh-agent.").muted(),
    ];
    if !agent_available {
        body.push(
            alert([
                alert_title("ssh-agent isn't reachable"),
                alert_description("SSH_AUTH_SOCK is not set in this environment."),
            ])
            .warning(),
        );
    }
    body.push(row([agent_button]).align(Align::Start));

    modal("wizard", "Set up your Rumble identity", body)
}

fn render_generate_local(password: &str, confirm: &str, error: Option<&str>, selection: &Selection) -> El {
    let mismatch = !password.is_empty() && !confirm.is_empty() && password != confirm;
    let can_generate = password.is_empty() || password == confirm;

    let mut body: Vec<El> = vec![
        paragraph(
            "Leave the password blank for plaintext storage. With a password, the key on disk is encrypted (Argon2 + \
             ChaCha20-Poly1305).",
        )
        .muted(),
        text("Password").muted(),
        password_input::password_input(password, selection, KEY_PWD),
        text("Confirm").muted(),
        password_input::password_input(confirm, selection, KEY_CONFIRM),
    ];

    if mismatch {
        body.push(form_message("Passwords don't match"));
    }
    if let Some(err) = error {
        body.push(form_message(err.to_string()));
    }

    let submit = if can_generate {
        button("Generate key").key(KEY_SUBMIT).primary()
    } else {
        button("Generate key").key(KEY_SUBMIT).disabled()
    };
    body.push(
        row([button("Back").key(KEY_BACK), spacer(), submit])
            .gap(tokens::SPACE_2)
            .width(Size::Fill(1.0))
            .align(Align::Center),
    );

    modal("wizard", "Generate local key", body)
}

fn render_connecting_agent() -> El {
    modal(
        "wizard",
        "Connecting to SSH agent",
        [
            row([spinner(), paragraph("Fetching Ed25519 keys from ssh-agent…").muted()])
                .gap(tokens::SPACE_2)
                .align(Align::Center),
            row([spacer(), button("Cancel").key(KEY_CANCEL_AGENT)])
                .width(Size::Fill(1.0))
                .align(Align::Center),
        ],
    )
}

fn render_select_agent_key(keys: &[KeyInfo], selected: Option<usize>, error: Option<&str>) -> El {
    let mut body: Vec<El> = Vec::new();

    if keys.is_empty() {
        body.push(paragraph("No Ed25519 keys are loaded in your SSH agent.").muted());
    } else {
        body.push(paragraph("Pick a key, or generate a new one and add it to the agent.").muted());
        body.push(item_group(
            keys.iter()
                .enumerate()
                .map(|(idx, key)| agent_key_row(idx, key, selected == Some(idx))),
        ));
    }

    if let Some(err) = error {
        body.push(form_message(err.to_string()));
    }

    let use_btn = if selected.is_some() {
        button("Use selected key").key(KEY_SUBMIT).primary()
    } else {
        button("Use selected key").key(KEY_SUBMIT).disabled()
    };

    // "Generate new agent key" overflows the 420 px modal when placed
    // alongside Back + Use selected key — drop the redundant "agent"
    // word (the modal title already establishes the context) so the
    // row fits inside the panel.
    body.push(
        row([
            button("Back").key(KEY_BACK),
            spacer(),
            button("Generate new key").key(KEY_GEN_AGENT_KEY),
            use_btn,
        ])
        .gap(tokens::SPACE_2)
        .width(Size::Fill(1.0))
        .align(Align::Center),
    );

    modal("wizard", "Select SSH agent key", body)
}

fn agent_key_row(idx: usize, key: &KeyInfo, selected: bool) -> El {
    let title = if key.comment.is_empty() {
        "Ed25519 key".to_string()
    } else {
        key.comment.clone()
    };
    let row_el = item([item_content([
        item_title(title),
        item_description(key.fingerprint.clone()),
    ])])
    .key(agent_row_key(idx));
    if selected { row_el.selected() } else { row_el }
}

fn render_generate_agent_key(comment: &str, busy: bool, selection: &Selection) -> El {
    let submit = if busy {
        button("Generating…").key(KEY_SUBMIT).disabled()
    } else {
        button("Generate and add").key(KEY_SUBMIT).primary()
    };
    let busy_indicator: Option<El> = busy.then(|| {
        row([spinner(), text("Adding key to ssh-agent…").muted()])
            .gap(tokens::SPACE_2)
            .align(Align::Center)
    });

    let mut body: Vec<El> = vec![
        paragraph("A new Ed25519 key will be generated and added to your ssh-agent.").muted(),
        form_item([
            form_label("Comment"),
            form_control(text_input(comment, selection, KEY_AGENT_COMMENT)),
        ]),
    ];
    if let Some(busy_row) = busy_indicator {
        body.push(busy_row);
    }
    body.push(
        row([button("Back").key(KEY_BACK), spacer(), submit])
            .gap(tokens::SPACE_2)
            .width(Size::Fill(1.0))
            .align(Align::Center),
    );

    modal("wizard", "Generate SSH agent key", body)
}

fn render_error(message: &str) -> El {
    modal(
        "wizard",
        "Identity setup failed",
        [
            alert([
                alert_title("Could not set up identity"),
                alert_description(message.to_string()),
            ])
            .destructive(),
            row([spacer(), button("Back").key(KEY_BACK).primary()])
                .width(Size::Fill(1.0))
                .align(Align::Center),
        ],
    )
}

pub fn render_unlock(state: &UnlockState, selection: &Selection) -> El {
    let mut body: Vec<El> = vec![
        paragraph("Enter the password for your encrypted Rumble identity.").muted(),
        password_input::password_input(&state.password, selection, KEY_UNLOCK_PWD),
    ];
    if let Some(err) = &state.error {
        body.push(form_message(err.clone()));
    }
    let submit = if state.password.is_empty() {
        button("Unlock").key(KEY_UNLOCK_SUBMIT).disabled()
    } else {
        button("Unlock").key(KEY_UNLOCK_SUBMIT).primary()
    };
    body.push(row([spacer(), submit]).width(Size::Fill(1.0)).align(Align::Center));
    modal("unlock", "Unlock identity", body)
}

// ============================================================
// Event handling
// ============================================================

pub fn handle_event(state: &mut WizardState, event: &UiEvent, selection: &mut Selection) -> WizardOutcome {
    match state {
        WizardState::NotNeeded | WizardState::Complete => return WizardOutcome::Ignored,
        WizardState::SelectMethod => {
            if event.is_click_or_activate(KEY_GEN_LOCAL) {
                *state = WizardState::GenerateLocal {
                    password: String::new(),
                    confirm: String::new(),
                    error: None,
                };
                return WizardOutcome::Handled;
            }
            if event.is_click_or_activate(KEY_USE_AGENT) {
                *state = WizardState::ConnectingAgent;
                return WizardOutcome::SpawnConnect;
            }
        }
        WizardState::GenerateLocal {
            password,
            confirm,
            error,
        } => {
            if event.target_key() == Some(KEY_PWD) {
                password_input::apply_event(password, selection, KEY_PWD, event);
                return WizardOutcome::Handled;
            }
            if event.target_key() == Some(KEY_CONFIRM) {
                password_input::apply_event(confirm, selection, KEY_CONFIRM, event);
                return WizardOutcome::Handled;
            }
            if event.is_click_or_activate(KEY_BACK) {
                *state = WizardState::SelectMethod;
                return WizardOutcome::Handled;
            }
            if event.is_click_or_activate(KEY_SUBMIT) {
                if password.is_empty() || password == confirm {
                    let pw_opt = (!password.is_empty()).then(|| password.clone());
                    return WizardOutcome::GenerateLocal { password: pw_opt };
                }
                *error = Some("Passwords don't match".to_string());
                return WizardOutcome::Handled;
            }
        }
        WizardState::ConnectingAgent => {
            if event.is_click_or_activate(KEY_CANCEL_AGENT) {
                *state = WizardState::SelectMethod;
                return WizardOutcome::Handled;
            }
        }
        WizardState::SelectAgentKey { keys, selected, .. } => {
            if event.is_click_or_activate(KEY_BACK) {
                *state = WizardState::SelectMethod;
                return WizardOutcome::Handled;
            }
            if event.is_click_or_activate(KEY_GEN_AGENT_KEY) {
                *state = WizardState::GenerateAgentKey {
                    comment: "rumble-identity".to_string(),
                };
                return WizardOutcome::Handled;
            }
            if event.is_click_or_activate(KEY_SUBMIT) {
                if let Some(idx) = *selected
                    && let Some(info) = keys.get(idx).cloned()
                {
                    return WizardOutcome::SelectAgentKey { key_info: info };
                }
                return WizardOutcome::Handled;
            }
            if matches!(event.kind, UiEventKind::Click | UiEventKind::Activate)
                && let Some(key) = event.route()
                && let Some(idx) = parse_agent_row_key(key)
            {
                *selected = Some(idx);
                return WizardOutcome::Handled;
            }
        }
        WizardState::GenerateAgentKey { comment } => {
            if event.target_key() == Some(KEY_AGENT_COMMENT) {
                text_input::apply_event(comment, selection, KEY_AGENT_COMMENT, event);
                return WizardOutcome::Handled;
            }
            if event.is_click_or_activate(KEY_BACK) {
                *state = WizardState::ConnectingAgent;
                return WizardOutcome::SpawnConnect;
            }
            if event.is_click_or_activate(KEY_SUBMIT) {
                let comment = comment.clone();
                return WizardOutcome::SpawnAddKey { comment };
            }
        }
        WizardState::Error { .. } => {
            if event.is_click_or_activate(KEY_BACK) {
                *state = WizardState::SelectMethod;
                return WizardOutcome::Handled;
            }
        }
    }
    WizardOutcome::Ignored
}

pub fn handle_unlock_event(state: &mut UnlockState, event: &UiEvent, selection: &mut Selection) -> WizardOutcome {
    if event.target_key() == Some(KEY_UNLOCK_PWD) {
        // Submit on bare Enter (no shift) — matches rumble-next's UX.
        if let UiEventKind::KeyDown = event.kind
            && let Some(kp) = event.key_press.as_ref()
            && matches!(kp.key, UiKey::Enter)
            && !kp.modifiers.shift
            && !state.password.is_empty()
        {
            return WizardOutcome::Unlock {
                password: state.password.clone(),
            };
        }
        password_input::apply_event(&mut state.password, selection, KEY_UNLOCK_PWD, event);
        return WizardOutcome::Handled;
    }
    if event.is_click_or_activate(KEY_UNLOCK_SUBMIT) && !state.password.is_empty() {
        return WizardOutcome::Unlock {
            password: state.password.clone(),
        };
    }
    WizardOutcome::Ignored
}

// ============================================================
// Outcome handlers (run on the App, with access to runtime + identity)
// ============================================================

/// Apply `Identity::generate_local_key`. Mutates `state` to the next
/// screen.
pub fn apply_generate_local(
    state: &mut WizardState,
    identity: &mut Identity,
    password: Option<String>,
) -> Option<KeyInfo> {
    match identity.generate_local_key(password.as_deref()) {
        Ok(info) => {
            *state = WizardState::Complete;
            Some(info)
        }
        Err(e) => {
            // Surface the error in-place so the user can retry.
            if let WizardState::GenerateLocal { error, .. } = state {
                *error = Some(format!("Failed to generate key: {e}"));
            } else {
                *state = WizardState::Error {
                    message: format!("Failed to generate key: {e}"),
                };
            }
            None
        }
    }
}

pub fn apply_select_agent_key(state: &mut WizardState, identity: &mut Identity, key_info: &KeyInfo) -> Option<KeyInfo> {
    match identity.select_agent_key(key_info) {
        Ok(()) => {
            *state = WizardState::Complete;
            Some(key_info.clone())
        }
        Err(e) => {
            *state = WizardState::Error {
                message: format!("Failed to save key config: {e}"),
            };
            None
        }
    }
}

pub fn apply_unlock(state: &mut UnlockState, identity: &mut Identity) -> bool {
    match identity.unlock(&state.password) {
        Ok(()) => {
            *state = UnlockState::default();
            true
        }
        Err(e) => {
            state.error = Some(e.to_string());
            false
        }
    }
}

fn ssh_agent_available() -> bool {
    rumble_desktop_shell::identity::SshAgentClient::is_available()
}
