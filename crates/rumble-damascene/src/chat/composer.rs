//! Chat composer: the multi-line input's ephemeral state (text buffer +
//! slash-command suggestion highlight) and the event handling for
//! slash-command navigation/completion, send-on-Enter, and Ctrl+V image
//! paste. The App owns one [`ChatComposerState`], routes composer events
//! through [`handle_event`], and acts on the returned [`ComposerOutcome`].

use damascene_core::prelude::*;
use rumble_client::{Command, State};

use super::{KEY_CMD_PREFIX, KEY_INPUT, command_suggestions};

/// Ephemeral composer state owned by the App.
#[derive(Default)]
pub struct ChatComposerState {
    /// Current composer text buffer.
    pub input: String,
    /// Index of the arrow-key-highlighted slash-command suggestion, if
    /// any. `None` is the default (Tab still completes the top row, Enter
    /// still sends). Reset to `None` on any text edit since the
    /// suggestion list is recomputed; always kept in range.
    pub command_selected: Option<usize>,
}

/// What the App should do after [`handle_event`].
pub enum ComposerOutcome {
    /// Not a composer event — the App should continue its event cascade.
    Ignored,
    /// Consumed; no App-side side effect.
    Handled,
    /// Send these commands (parsed from a submitted composer line).
    Send(Vec<Command>),
    /// Ctrl+V with no clipboard text — try an image paste (App-side).
    PasteImage,
}

/// Complete the composer line to `/<name> ` with the caret at the end,
/// ready for arguments. The trailing space hides the suggestion list.
fn complete(state: &mut ChatComposerState, selection: &mut Selection, name: &str) {
    let text = format!("/{name} ");
    let caret = text.len();
    state.input = text;
    *selection = Selection::caret(KEY_INPUT, caret);
    state.command_selected = None;
}

/// Parse a submitted composer line into the commands to send. Slash
/// commands (`/msg`, `/tree`) dispatch their dedicated variants; usage
/// errors become `Command::LocalMessage`. Plain text becomes
/// `Command::SendChat`. Returns the commands in send order (always one
/// element in practice).
pub fn parse_chat(raw: &str, app_state: &State) -> Vec<Command> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    if trimmed == "/msg" || trimmed.starts_with("/msg ") {
        let rest = trimmed.strip_prefix("/msg").unwrap().trim_start();
        return match rest.split_once(' ') {
            Some((target_name, body)) => {
                let body = body.trim();
                if body.is_empty() {
                    return vec![Command::LocalMessage {
                        text: "Usage: /msg <username> <message>".to_string(),
                    }];
                }
                match app_state.users.iter().find(|u| u.username == target_name) {
                    Some(user) => {
                        let uid = user.user_id.as_ref().map(|id| id.value).unwrap_or(0);
                        vec![Command::SendDirectMessage {
                            target_user_id: uid,
                            target_username: target_name.to_string(),
                            text: body.to_string(),
                        }]
                    }
                    None => vec![Command::LocalMessage {
                        text: format!("User '{target_name}' not found"),
                    }],
                }
            }
            None => vec![Command::LocalMessage {
                text: "Usage: /msg <username> <message>".to_string(),
            }],
        };
    }
    if trimmed == "/tree" || trimmed.starts_with("/tree ") {
        let rest = trimmed.strip_prefix("/tree").unwrap().trim_start();
        return if rest.is_empty() {
            vec![Command::LocalMessage {
                text: "Usage: /tree <message>".to_string(),
            }]
        } else {
            vec![Command::SendTreeChat { text: rest.to_string() }]
        };
    }
    vec![Command::SendChat {
        text: trimmed.to_string(),
    }]
}

/// Route a UI event through the composer. Returns [`ComposerOutcome::Ignored`]
/// for anything that isn't a composer event so the App can continue its
/// cascade.
pub fn handle_event(
    state: &mut ChatComposerState,
    selection: &mut Selection,
    event: &UiEvent,
    app_state: &State,
) -> ComposerOutcome {
    // Slash-command suggestion picked (clicked) from the composer list.
    if let Some(key) = event.target_key()
        && key.starts_with(KEY_CMD_PREFIX)
        && event.is_click_or_activate(key)
    {
        let name = key[KEY_CMD_PREFIX.len()..].to_string();
        complete(state, selection, &name);
        return ComposerOutcome::Handled;
    }

    if event.target_key() != Some(KEY_INPUT) {
        return ComposerOutcome::Ignored;
    }

    // Slash-command suggestion navigation/completion. The list only
    // shows for a single-line `/command` fragment (no whitespace), so
    // intercepting Up/Down here never steals multiline caret movement.
    if let UiEventKind::KeyDown = event.kind
        && let Some(kp) = event.key_press.as_ref()
    {
        let suggestions = command_suggestions(&state.input, app_state);
        if !suggestions.is_empty() {
            let n = suggestions.len();
            // Clamp any stale highlight (defensive) so the index is in range.
            let sel = state.command_selected.filter(|i| *i < n);
            match kp.key {
                UiKey::ArrowDown if !kp.modifiers.shift => {
                    state.command_selected = Some(sel.map_or(0, |i| (i + 1) % n));
                    return ComposerOutcome::Handled;
                }
                UiKey::ArrowUp if !kp.modifiers.shift => {
                    state.command_selected = Some(sel.map_or(n - 1, |i| (i + n - 1) % n));
                    return ComposerOutcome::Handled;
                }
                UiKey::Escape if sel.is_some() => {
                    state.command_selected = None;
                    return ComposerOutcome::Handled;
                }
                UiKey::Tab if !kp.modifiers.shift => {
                    let name = suggestions[sel.unwrap_or(0)].0.clone();
                    complete(state, selection, &name);
                    return ComposerOutcome::Handled;
                }
                UiKey::Enter
                    if sel.is_some()
                        && !kp.modifiers.shift
                        && !kp.modifiers.ctrl
                        && !kp.modifiers.alt
                        && !kp.modifiers.logo =>
                {
                    let name = suggestions[sel.unwrap()].0.clone();
                    complete(state, selection, &name);
                    return ComposerOutcome::Handled;
                }
                _ => {}
            }
        }
    }

    // Send on bare Enter. Shift+Enter / Ctrl+J fall through to
    // `text_area::apply_event` for newline insertion.
    if let UiEventKind::KeyDown = event.kind
        && let Some(kp) = event.key_press.as_ref()
        && matches!(kp.key, UiKey::Enter)
        && !kp.modifiers.shift
        && !kp.modifiers.ctrl
        && !kp.modifiers.alt
        && !kp.modifiers.logo
    {
        let trimmed = state.input.trim().to_string();
        if !trimmed.is_empty() {
            let cmds = parse_chat(&trimmed, app_state);
            state.input.clear();
            *selection = Selection::default();
            state.command_selected = None;
            return ComposerOutcome::Send(cmds);
        }
        return ComposerOutcome::Handled;
    }

    // Ctrl/Cmd+V with (presumably) an image on the clipboard. The host
    // only forwards the raw paste keypress when clipboard text was empty,
    // so reaching here means no text — let the App try an image paste.
    if let UiEventKind::KeyDown = event.kind
        && let Some(kp) = event.key_press.as_ref()
        && let UiKey::Character(c) = &kp.key
        && c.eq_ignore_ascii_case("v")
        && (kp.modifiers.ctrl || kp.modifiers.logo)
        && !kp.modifiers.shift
        && !kp.modifiers.alt
    {
        return ComposerOutcome::PasteImage;
    }

    text_area::apply_event(&mut state.input, selection, KEY_INPUT, event);
    // The text (and thus the suggestion list) just changed; drop any
    // arrow-key highlight so a stale index never points at the wrong row.
    state.command_selected = None;
    ComposerOutcome::Handled
}
