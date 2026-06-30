//! Kick / ban prompt.
//!
//! Opened from a connected user's context menu (gated on the root-scoped
//! `KICK` / `BAN` permissions). Collects an optional reason and — for bans —
//! a duration, then dispatches `Command::KickUser` / `Command::BanUser`. Both
//! target a live session id, so this only appears for connected remote users.
//! The server replies with a `CommandResult` surfaced as a local chat notice.
//!
//! Unban is not handled here — a ban is membership in the standard `banned`
//! group, lifted from the Admin tab's group editor (see `admin.rs`).

use damascene_core::prelude::*;

use rumble_client::Command;

/// Which action the modal is collecting input for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModerationKind {
    Kick,
    Ban,
}

/// Ban-duration presets. `Permanent` maps to `duration_seconds = None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BanDuration {
    #[default]
    Permanent,
    Hour,
    Day,
    Week,
}

impl BanDuration {
    const ALL: &'static [BanDuration] = &[
        BanDuration::Permanent,
        BanDuration::Hour,
        BanDuration::Day,
        BanDuration::Week,
    ];

    fn slug(self) -> &'static str {
        match self {
            BanDuration::Permanent => "perm",
            BanDuration::Hour => "1h",
            BanDuration::Day => "1d",
            BanDuration::Week => "1w",
        }
    }

    fn label(self) -> &'static str {
        match self {
            BanDuration::Permanent => "Permanent",
            BanDuration::Hour => "1 hour",
            BanDuration::Day => "1 day",
            BanDuration::Week => "1 week",
        }
    }

    fn from_slug(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|d| d.slug() == s)
    }

    /// `duration_seconds` for `Command::BanUser` — `None` for a permanent ban.
    fn seconds(self) -> Option<u64> {
        match self {
            BanDuration::Permanent => None,
            BanDuration::Hour => Some(3_600),
            BanDuration::Day => Some(86_400),
            BanDuration::Week => Some(604_800),
        }
    }
}

#[derive(Debug)]
pub struct KickBanModal {
    pub user_id: u64,
    pub username: String,
    pub kind: ModerationKind,
    pub reason: String,
    /// Only meaningful for [`ModerationKind::Ban`].
    pub duration: BanDuration,
}

impl KickBanModal {
    pub fn kick(user_id: u64, username: String) -> Self {
        Self {
            user_id,
            username,
            kind: ModerationKind::Kick,
            reason: String::new(),
            duration: BanDuration::default(),
        }
    }

    pub fn ban(user_id: u64, username: String) -> Self {
        Self {
            user_id,
            username,
            kind: ModerationKind::Ban,
            reason: String::new(),
            duration: BanDuration::default(),
        }
    }

    fn command(&self) -> Command {
        let reason = self.reason.trim().to_string();
        match self.kind {
            ModerationKind::Kick => Command::KickUser {
                target_user_id: self.user_id,
                reason,
            },
            ModerationKind::Ban => Command::BanUser {
                target_user_id: self.user_id,
                reason,
                duration_seconds: self.duration.seconds(),
            },
        }
    }
}

pub enum ModerationOutcome {
    /// Event wasn't for this modal.
    Ignored,
    /// Event consumed; no further action.
    Handled,
    /// Cancel / Escape / scrim — close the modal.
    Cancel,
    /// Confirm — caller fires the carried command and closes the modal.
    Submit(Command),
}

const KEY_DISMISS: &str = "moderation:dismiss";
const KEY_REASON: &str = "moderation:reason";
const KEY_DURATION: &str = "moderation:duration";
const KEY_CANCEL: &str = "moderation:cancel";
const KEY_CONFIRM: &str = "moderation:confirm";

pub fn render(state: &KickBanModal, selection: &Selection) -> El {
    let (title, verb, lead) = match state.kind {
        ModerationKind::Kick => (
            format!("Kick {}", state.username),
            "Kick",
            "Disconnect this user from the server. They can reconnect immediately.",
        ),
        ModerationKind::Ban => (
            format!("Ban {}", state.username),
            "Ban",
            "Add this user's key to the banned group and disconnect them. They can't reconnect until unbanned.",
        ),
    };

    let mut body: Vec<El> = vec![
        paragraph(lead).muted().font_size(tokens::TEXT_XS.size),
        field_row(
            "Reason (optional)",
            text_input_with(
                KEY_REASON,
                &state.reason,
                selection,
                TextInputOpts {
                    placeholder: Some("Shown to the user"),
                    ..Default::default()
                },
            )
            .width(Size::Fill(1.0)),
        ),
    ];

    if state.kind == ModerationKind::Ban {
        // Stacked (label above full-width tabs) rather than field_row — four
        // text labels don't fit in field_row's half-width control column.
        body.push(
            column([
                text("Duration").muted().font_size(tokens::TEXT_XS.size),
                tabs_list(
                    KEY_DURATION,
                    &state.duration.slug(),
                    BanDuration::ALL.iter().map(|d| (d.slug(), d.label())),
                )
                .width(Size::Fill(1.0)),
            ])
            .gap(tokens::SPACE_1)
            .width(Size::Fill(1.0)),
        );
    }

    body.push(
        row([
            button("Cancel").key(KEY_CANCEL),
            spacer(),
            button(verb).key(KEY_CONFIRM).destructive(),
        ])
        .gap(tokens::SPACE_2)
        .width(Size::Fill(1.0))
        .align(Align::Center),
    );

    // Built explicitly (rather than `modal()`) so the panel is wide enough for
    // the four duration tabs — the 420px default clips "Permanent".
    overlay([
        scrim(KEY_DISMISS),
        modal_panel(title, body).width(Size::Fixed(480.0)).block_pointer(),
    ])
}

pub fn handle_event(state: &mut KickBanModal, event: &UiEvent, selection: &mut Selection) -> ModerationOutcome {
    if event.is_click_or_activate(KEY_DISMISS) || event.is_click_or_activate(KEY_CANCEL) {
        return ModerationOutcome::Cancel;
    }
    if let UiEventKind::KeyDown = event.kind
        && let Some(kp) = event.key_press.as_ref()
        && kp.logical.named() == Some(NamedKey::Escape)
    {
        return ModerationOutcome::Cancel;
    }

    // Reason text input.
    if event.target_key() == Some(KEY_REASON) {
        damascene_core::widgets::text_input::apply_event(&mut state.reason, selection, event, KEY_REASON);
        return ModerationOutcome::Handled;
    }

    // Ban-duration tabs.
    if state.kind == ModerationKind::Ban
        && damascene_core::widgets::tabs::apply_event(&mut state.duration, event, KEY_DURATION, BanDuration::from_slug)
    {
        return ModerationOutcome::Handled;
    }

    if event.is_click_or_activate(KEY_CONFIRM) {
        return ModerationOutcome::Submit(state.command());
    }

    ModerationOutcome::Ignored
}
