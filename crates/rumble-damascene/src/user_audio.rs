//! Per-user audio panel.
//!
//! Opened from a remote user's context menu ("Audio…") in the room
//! tree. Everything here is **local playback** configuration — it only
//! changes what *we* hear from that user, nothing is sent to the
//! server — so there is no permission gating.
//!
//! Two controls, mirroring [`rumble_client::UserRxConfig`]:
//!
//! - **Volume** (`volume_db`) — commits via [`Command::SetUserVolume`]
//!   on gesture end (pointer release / keyboard nudge), never per
//!   drag frame. `SetUserVolume` is cheap on the audio task (no
//!   pipeline rebuild), so committing it live gives immediate audible
//!   feedback while someone is talking.
//! - **Custom RX pipeline** (`pipeline_override`) — edited with the
//!   shared [`crate::pipeline_ui`] editor against pending state and
//!   committed via [`Command::UpdateUserRxConfig`] on Apply/Save,
//!   because every commit rebuilds the user's pipeline. "Reset to
//!   default" fires [`Command::ClearUserRxOverride`] immediately (a
//!   one-shot side effect, like Settings' "Refresh devices").
//!
//! Like the other App-owned modals the struct survives re-render: the
//! App keeps `Option<UserAudioModal>` and reprojects it every frame.
//! All commands are handled by the always-running audio task, so they
//! are safe no-ops server-wise while disconnected (the panel is only
//! reachable from the room tree, which requires a session anyway, and
//! the App drops it on disconnect).

use damascene_core::prelude::*;
use rumble_client::{Command, PipelineConfig, ProcessorRegistry, State, UserRxConfig};

use crate::pipeline_ui::{self, PipelineEvent, PipelineKeys, section_card};

/// Volume slider range. ±30 dB matches Mumble's local volume
/// adjustment span; 0 dB (the default) is unity gain.
const VOLUME_MIN_DB: f32 = -30.0;
const VOLUME_MAX_DB: f32 = 30.0;
/// Values this close to unity snap to exactly 0 dB so a drag released
/// near the midpoint reads (and persists) as "no adjustment".
const VOLUME_SNAP_DB: f32 = 0.5;

const KEY_DISMISS: &str = "uaudio:dismiss";
const KEY_CLOSE: &str = "uaudio:close";
const KEY_APPLY: &str = "uaudio:apply";
const KEY_SAVE: &str = "uaudio:save";
const KEY_RESET: &str = "uaudio:reset";
const KEY_VOLUME: &str = "uaudio:volume";
const KEY_OVERRIDE: &str = "uaudio:override";

/// Routed-key namespace for the override pipeline editor. Distinct
/// from the Settings dialog's `settings:proc:` / `settings:rxproc:`
/// namespaces so the two surfaces can never route into each other.
const PIPELINE_KEYS: PipelineKeys = PipelineKeys {
    prefix: "uaudio:proc:",
    add_key: "uaudio:proc-add",
};

/// Ephemeral state for the panel. Seeded from
/// `state.audio.per_user_rx` when opened; volume edits commit on
/// gesture end, pipeline edits on Apply/Save.
#[derive(Debug)]
pub struct UserAudioModal {
    pub user_id: u64,
    pub username: String,
    /// Pending volume in dB. Committed live (on release), so this
    /// usually matches the audio task's value except mid-drag.
    pub volume_db: f32,
    /// Pending pipeline override. `None` = follow the RX defaults from
    /// Settings→Processing.
    pub pipeline_override: Option<PipelineConfig>,
    /// Whether the "Add processor" dropdown is open.
    pub add_menu_open: bool,
}

impl UserAudioModal {
    /// Snapshot the user's current RX config into pending state.
    /// Returns `None` when the user id isn't in the current state
    /// (e.g. they disconnected between menu-open and click).
    pub fn open_for(app_state: &State, user_id: u64) -> Option<Self> {
        let user = app_state
            .users
            .iter()
            .find(|u| u.user_id.as_ref().map(|i| i.value) == Some(user_id))?;
        let config = app_state.audio.per_user_rx.get(&user_id).cloned().unwrap_or_default();
        Some(Self {
            user_id,
            username: user.username.clone(),
            volume_db: config.volume_db,
            pipeline_override: config.pipeline_override,
            add_menu_open: false,
        })
    }

    /// Commands that make the audio task match the pending state. A
    /// fully-default pending state clears the override entry outright
    /// instead of storing an all-defaults config, so `per_user_rx`
    /// only ever holds real overrides.
    fn commit_commands(&self) -> Vec<Command> {
        if self.volume_db == 0.0 && self.pipeline_override.is_none() {
            vec![Command::ClearUserRxOverride { user_id: self.user_id }]
        } else {
            vec![Command::UpdateUserRxConfig {
                user_id: self.user_id,
                config: UserRxConfig {
                    volume_db: self.volume_db,
                    pipeline_override: self.pipeline_override.clone(),
                },
            }]
        }
    }

    fn set_volume_normalized(&mut self, n: f32) {
        let v = VOLUME_MIN_DB + n.clamp(0.0, 1.0) * (VOLUME_MAX_DB - VOLUME_MIN_DB);
        self.volume_db = if v.abs() < VOLUME_SNAP_DB { 0.0 } else { v };
    }

    fn volume_normalized(&self) -> f32 {
        ((self.volume_db - VOLUME_MIN_DB) / (VOLUME_MAX_DB - VOLUME_MIN_DB)).clamp(0.0, 1.0)
    }
}

/// Outcome of routing a single event into the panel.
pub enum UserAudioOutcome {
    /// Not ours — but the App swallows it anyway (modal-first capture).
    Ignored,
    /// Consumed, no commands.
    Handled,
    /// Close without further commands.
    Close,
    /// Fire these commands and keep the panel open (live volume
    /// commit, Reset, Apply).
    Dispatch(Vec<Command>),
    /// Fire these commands and close (Save).
    Submit(Vec<Command>),
}

/// Render the panel and (when open) the add-processor dropdown as a
/// separate layer, so the popover paints above the modal panel it is
/// anchored to — same two-layer contract as the Settings dialog.
pub fn render(state: &UserAudioModal, registry: &ProcessorRegistry, selection: &Selection) -> (El, Option<El>) {
    let mut cards: Vec<El> = Vec::new();

    cards.push(
        paragraph("Local playback settings for this user — they only change what you hear.")
            .muted()
            .font_size(tokens::TEXT_XS.size),
    );

    // Volume: label carries the live value so the slider needs no
    // separate readout.
    cards.push(section_card(
        "Volume",
        [
            field_row(
                format!("Volume ({:+.1} dB)", state.volume_db),
                slider(KEY_VOLUME, state.volume_normalized()).width(Size::Fixed(220.0)),
            ),
            paragraph("0 dB leaves the user's volume unchanged. Applies immediately.")
                .muted()
                .font_size(tokens::TEXT_XS.size),
        ],
    ));

    // Pipeline override. The switch swaps between "follow defaults"
    // and a private copy of the pipeline seeded from those defaults.
    let mut override_body: Vec<El> = vec![
        field_row(
            "Custom pipeline",
            switch(KEY_OVERRIDE, state.pipeline_override.is_some()),
        ),
        paragraph(if state.pipeline_override.is_some() {
            "This user has their own processing chain; edits below apply only to them."
        } else {
            "Off — this user follows the incoming-audio defaults from Settings → Processing."
        })
        .muted()
        .font_size(tokens::TEXT_XS.size),
    ];
    if state.pipeline_override.is_some() {
        override_body.push(
            paragraph("Pipeline changes land when you press Apply or Save.")
                .muted()
                .font_size(tokens::TEXT_XS.size),
        );
    }
    cards.push(section_card("Processing override", override_body));

    if let Some(pipeline) = state.pipeline_override.as_ref() {
        cards.extend(pipeline_ui::render_stage_cards(
            &PIPELINE_KEYS,
            pipeline,
            registry,
            selection,
            |_, _| Vec::new(),
        ));
        cards.push(pipeline_ui::render_add_card(&PIPELINE_KEYS));
    }

    let footer = row([
        button("Reset to default").key(KEY_RESET).secondary(),
        spacer(),
        button("Close").key(KEY_CLOSE),
        button("Apply").key(KEY_APPLY).secondary(),
        button("Save").key(KEY_SAVE).primary(),
    ])
    .gap(tokens::SPACE_2)
    .width(Size::Fill(1.0))
    .align(Align::Center);

    // Fixed-height panel with an inner scroll region: the override
    // editor can grow arbitrarily tall (one card per stage), and the
    // footer has to stay reachable.
    let panel = modal_panel(
        format!("Audio: {}", state.username),
        [
            scroll([column(cards).gap(tokens::SPACE_3).width(Size::Fill(1.0))])
                .padding(Sides {
                    left: 0.0,
                    right: 0.0,
                    top: tokens::SPACE_2,
                    bottom: tokens::SPACE_2,
                })
                .scrollbar_gutter()
                .gap(tokens::SPACE_3)
                .width(Size::Fill(1.0))
                .height(Size::Fill(1.0)),
            divider(),
            footer,
        ],
    )
    .width(Size::Fixed(560.0))
    .height(Size::Fixed(520.0));

    let panel_layer = overlay([scrim(KEY_DISMISS), panel.block_pointer()]);

    let popover_layer = if state.add_menu_open && state.pipeline_override.is_some() {
        Some(pipeline_ui::render_add_menu(&PIPELINE_KEYS, registry))
    } else {
        None
    };

    (panel_layer, popover_layer)
}

/// Route a single event into the panel. The App calls this modal-first
/// and swallows everything while the panel is open (same capture rule
/// as the kick/ban and elevate modals).
pub fn handle_event(
    state: &mut UserAudioModal,
    event: &UiEvent,
    app_state: &State,
    registry: &ProcessorRegistry,
    selection: &mut Selection,
) -> UserAudioOutcome {
    // Escape backs out of the dropdown first, then the panel — same
    // two-step as the Settings dialog.
    if event.kind == UiEventKind::Escape {
        if state.add_menu_open {
            state.add_menu_open = false;
            return UserAudioOutcome::Handled;
        }
        return UserAudioOutcome::Close;
    }
    if (event.is_route(KEY_DISMISS) && event.kind == UiEventKind::Click) || event.is_click_or_activate(KEY_CLOSE) {
        return UserAudioOutcome::Close;
    }

    // Override pipeline editor (add dropdown + per-stage controls).
    // Routed before the plain controls below so the dropdown's scrim
    // click can't fall through to the panel.
    if let Some(pipeline) = state.pipeline_override.as_mut() {
        match pipeline_ui::handle_event(&PIPELINE_KEYS, event, pipeline, registry, selection) {
            PipelineEvent::Ignored => {}
            PipelineEvent::Changed => return UserAudioOutcome::Handled,
            PipelineEvent::AddToggle => {
                state.add_menu_open = !state.add_menu_open;
                return UserAudioOutcome::Handled;
            }
            PipelineEvent::AddDismiss | PipelineEvent::AddPicked => {
                state.add_menu_open = false;
                return UserAudioOutcome::Handled;
            }
        }
    }

    // "Custom pipeline" switch. Turning it on seeds the private copy
    // from the *current* RX defaults so the user starts from what they
    // already hear; turning it off reverts to following the defaults
    // (committed on Apply/Save).
    {
        let mut enabled = state.pipeline_override.is_some();
        if switch::apply_event(&mut enabled, event, KEY_OVERRIDE) {
            state.pipeline_override = if enabled {
                Some(app_state.audio.rx_pipeline_defaults.clone())
            } else {
                None
            };
            state.add_menu_open = false;
            return UserAudioOutcome::Handled;
        }
    }

    // Volume slider. Pointer drags only move the pending value (visual
    // feedback); the backend command fires once on release. Clicks and
    // keyboard nudges are single discrete edits, so they commit
    // immediately. This is the "no command per drag frame" rule every
    // other slider in the app follows.
    if event.is_route(KEY_VOLUME) {
        match event.kind {
            UiEventKind::PointerDown | UiEventKind::Drag => {
                if let (Some(rect), Some(x)) = (event.target_rect(), event.pointer_x()) {
                    state.set_volume_normalized(slider::normalized_from_event(rect, x));
                    return UserAudioOutcome::Handled;
                }
            }
            UiEventKind::PointerUp | UiEventKind::Click => {
                if let (Some(rect), Some(x)) = (event.target_rect(), event.pointer_x()) {
                    state.set_volume_normalized(slider::normalized_from_event(rect, x));
                    return UserAudioOutcome::Dispatch(vec![Command::SetUserVolume {
                        user_id: state.user_id,
                        volume_db: state.volume_db,
                    }]);
                }
            }
            _ => {}
        }
        // Keyboard nudge (arrows / Home / End) — normalized steps of
        // 5% / 25%, matching the Settings sliders.
        let mut n = state.volume_normalized();
        if slider::apply_event(&mut n, event, KEY_VOLUME, 0.05, 0.25) {
            state.set_volume_normalized(n);
            return UserAudioOutcome::Dispatch(vec![Command::SetUserVolume {
                user_id: state.user_id,
                volume_db: state.volume_db,
            }]);
        }
    }

    // Reset: one-shot side effect — clears the audio task's override
    // immediately and re-seeds pending to match.
    if event.is_click_or_activate(KEY_RESET) {
        state.volume_db = 0.0;
        state.pipeline_override = None;
        state.add_menu_open = false;
        return UserAudioOutcome::Dispatch(vec![Command::ClearUserRxOverride { user_id: state.user_id }]);
    }

    if event.is_click_or_activate(KEY_APPLY) {
        return UserAudioOutcome::Dispatch(state.commit_commands());
    }
    if event.is_click_or_activate(KEY_SAVE) {
        return UserAudioOutcome::Submit(state.commit_commands());
    }

    UserAudioOutcome::Ignored
}
