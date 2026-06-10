//! Top-level damascene `App` for the Rumble client.
//!
//! Owns local UI state (connect form fields, modal flags, selected
//! room) and projects `(state, ui_state) -> El` on every frame.

use std::{
    cell::Cell,
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::LazyLock,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use damascene_core::{
    prelude::*,
    scroll::{ScrollAlignment, ScrollRequest},
    toast::ToastSpec,
};
use damascene_winit_wgpu::WinitWgpuApp;

use rumble_client::{
    AudioSettings, ChatMessage, Command, ConnectionState, PendingCertificate, ProcessorRegistry, SfxKind, State,
    VoiceMode, build_default_tx_pipeline, merge_with_default_tx_pipeline, register_builtin_processors,
};
use rumble_desktop_shell::{
    AcceptedCertificate, RecentServer, SettingsStore,
    hotkeys::{HotkeyEvent, HotkeyManager},
    identity::{connect_and_list_keys, generate_and_add_to_agent},
};
use tokio::{runtime::Runtime, task::JoinHandle};

use crate::{
    backend::UiBackend,
    chat,
    elevate::{self, ElevateOutcome, ElevateState},
    identity::Identity,
    lightbox, model, room_acl,
    room_tree::{self, RoomTreeOutcome, RoomTreeState},
    server_picker::{self, ServerForm, ServerPickerOutcome, ServerPickerState},
    settings::{self, SettingsOutcome, SettingsState},
    video,
    wizard::{self, PendingAgentOp, UnlockState, WizardOutcome, WizardState},
};

/// Result yielded by `pending_video_open`: `(transfer_id, file_name, stream)` or libmpv error.
pub type PendingVideoOpenResult = Result<(String, String, rumble_video::VideoStream), rumble_video::Error>;

pub struct RumbleApp<B: UiBackend = crate::backend::NativeUiBackend> {
    backend: B,
    identity: Identity,
    settings: SettingsStore,

    /// Tokio runtime for spawning ssh-agent ops and other async work
    /// that needs to outlive a single event handler. The wizard polls
    /// `pending_agent_op.is_finished()` each frame and `block_on`s the
    /// completed handle to land the result on the same frame.
    runtime: Runtime,

    /// Global hotkey service. Drives PTT / mute / deafen from the
    /// user's persisted bindings, drained each frame in
    /// [`Self::pump_hotkeys`]. Initialised against `runtime` so the
    /// XDG portal backend (Wayland) shares the same async context.
    hotkeys: HotkeyManager,

    /// First-run identity wizard. `NotNeeded` when an identity is
    /// already configured.
    wizard: WizardState,
    /// Encrypted-key unlock prompt state. Only shown when
    /// `identity.needs_unlock()` is true and the wizard is hidden.
    unlock: UnlockState,
    /// Sudo / superuser elevation prompt. `Some` while the modal is
    /// open; cleared on Cancel/Escape/scrim or on submit. Result lands
    /// asynchronously as a `CommandResult` chat line and (on success)
    /// a `UserStatusChanged.is_elevated=true` broadcast.
    elevate: Option<ElevateState>,
    /// In-flight ssh-agent op spawned on `runtime`.
    pending_agent_op: Option<PendingAgentOp>,

    // ---- Local UI state ----
    /// Saved-server picker (disconnected center area + add/edit form).
    /// See [`crate::server_picker`].
    server_picker: ServerPickerState,
    settings_state: SettingsState,
    /// Open flag for the toolbar transmission-mode dropdown. Drives the
    /// popover layer composed below the trigger; cleared by Escape,
    /// scrim click, or picking an option.
    voice_mode_menu_open: bool,
    /// Force the unlock prompt visible regardless of `needs_unlock()`.
    /// Set by `set_unlock_state_for_test` so `dump_bundles` can render
    /// the prompt against a fresh on-disk identity that isn't actually
    /// encrypted.
    force_unlock_for_test: bool,
    /// Username pre-filled when adding a brand-new server entry.
    /// Sourced from `$USER` at startup, then refreshed to the most
    /// recently saved server form's username so adding a sequence of
    /// servers carries the same name. Per-server usernames live on each
    /// `RecentServer` (edited via the server form) and are authoritative
    /// at connect time; this is purely a form-prefill convenience.
    default_username: String,
    /// Global text-selection slot. Every `text_input` reads its caret /
    /// selection band through `selection.within(key)`; `apply_event`
    /// folds keypresses + clicks back into this single field.
    selection: Selection,

    /// Chat composer state (text buffer + slash-command highlight). See
    /// [`crate::chat::composer`].
    composer: chat::composer::ChatComposerState,

    /// Proportional split weights `[chat, tree]` for the main row divider.
    /// Dragging the handle redistributes between the two; each panel
    /// keeps at least 15% of the row. Default `[1.0, 2.5]` gives the
    /// chat pane roughly 28% on a fresh launch.
    chat_weights: [f32; 2],
    chat_sidebar_drag: ResizeWeightsDrag,
    /// Row pixel width captured last frame via `BuildCx::viewport()`.
    /// Used by `apply_event_weights` to convert pointer deltas to
    /// weight deltas.  Stored in a `Cell` so `build(&self)` can
    /// refresh it without requiring `&mut self`.
    chat_row_w: Cell<f32>,

    /// Audio-processor factory registry. Owned by the App so the
    /// settings dialog can read each processor's display name,
    /// description and JSON schema when rendering the Processing tab.
    /// Built once in [`Self::new`] from `register_builtin_processors`.
    processor_registry: ProcessorRegistry,

    /// Room tree view + its ephemeral state (selection, context menus,
    /// drag-and-drop, confirmation modals). See [`crate::room_tree`].
    room_tree: RoomTreeState,

    /// Id set of every chat message observed on the previous frame.
    /// `None` until the first non-empty observation, which seeds the set
    /// silently so the connect-time backlog isn't treated as new arrivals.
    /// After seeding, held as `Some` so id-based diffing can detect new
    /// messages even when the 100-entry sliding window caps the list length.
    /// See [`diff_new_chat_messages`].
    seen_chat_ids: Option<HashSet<[u8; 16]>>,

    /// Row key the chat-history virtual list should stick to bottom on next
    /// `drain_scroll_requests`. Set when the backlog grows (new message, initial
    /// load); consumed into a `ScrollRequest::ToRowKey(.., End)`. Virtual lists
    /// don't auto-pin like `scroll().pin_end()`, so we drive it explicitly.
    pending_scroll_to_bottom: Option<String>,

    /// Sound-effect edge detector. Owns the previous-frame snapshot for
    /// connect/mute/room transitions; see [`crate::sfx::SfxEngine`].
    sfx: crate::sfx::SfxEngine,

    /// In-flight OS file picker, spawned on `runtime` when the user
    /// clicks the share-file button. `Some(handle)` while the dialog is
    /// open; `before_build` polls and dispatches `Command::ShareFile`
    /// once the user picks a path (or drops the result on cancel).
    pending_file_dialog: Option<JoinHandle<Option<PathBuf>>>,

    /// `transfer_id`s for incoming `FileOffer` attachments we have
    /// already routed through the auto-download flow this session.
    /// Mid-session reconnects (or `RequestChatHistory`) re-emit the
    /// same offers; without this guard each replay would kick off
    /// another download. Cleared on disconnect.
    auto_handled_offers: HashSet<String>,

    /// Event-driven owner of every transfer-keyed media map (image
    /// cache, GIF playback, GPU mirrors, video thumbs, lightbox
    /// decode). Replaces what used to be eight parallel maps on App.
    /// Reacts to [`rumble_client::BackendEvent::TransferStageChanged`]
    /// drained each frame in `drain_backend_events`.
    media_cache: crate::media_cache::MediaCache,

    /// The three mutually-exclusive media lightboxes (image / video /
    /// 3D model). See [`crate::lightbox::Lightboxes`].
    lightboxes: crate::lightbox::Lightboxes,

    /// Right-click context menu for a file card. `Some` while open;
    /// cleared by any menu action, the dismiss scrim, or Escape.
    file_context_menu: Option<chat::FileContextMenu>,

    /// In-flight "Save As" dialog. When the handle completes the App
    /// copies the source file to the user-chosen destination.
    pending_save_as: Option<(PathBuf, JoinHandle<Option<PathBuf>>)>,

    /// In-flight folder picker for the Settings > Files download
    /// location. The result lands in `settings_state.pending` so the
    /// dialog stays open with the user's choice already filled in.
    pending_pick_download_dir: Option<JoinHandle<Option<PathBuf>>>,

    /// True while the OS reports a file is being dragged over the
    /// window. Drives the drop-target overlay; cleared on
    /// `HoveredFileCancelled` or when the drop lands.
    file_drop_hover: bool,

    /// Per-room Permissions editor modal. `Some` between the user
    /// picking "Permissions…" from the room context menu and the
    /// Save / Cancel that closes the modal. See [`crate::room_acl`].
    room_acl_modal: Option<room_acl::RoomAclModalState>,

    /// `wgpu::Device` handle stashed by [`WinitWgpuApp::gpu_setup`]
    /// once at startup, used in `media_cache.sync_animated_gpu` to
    /// lazily allocate per-message textures for animated previews.
    /// `None` before the host has finished bringing up wgpu (e.g.
    /// while running tests against a `MockUiBackend`).
    gpu_device: Option<wgpu::Device>,

    /// Queued toasts collected from backend events each frame.
    /// Drained by [`App::drain_toasts`] so damascene's runtime synthesizes
    /// the toast stack overlay automatically.
    pending_toasts: Vec<ToastSpec>,

    /// URLs captured from `UiEventKind::LinkActivated` since the last
    /// frame. Drained by [`App::drain_link_opens`]; the host
    /// (damascene-winit-wgpu) routes each one through the OS opener.
    pending_link_opens: Vec<String>,

    /// Transfer ids whose cancel button has been clicked once and is
    /// awaiting confirmation. Second click fires the actual cancel.
    /// Entries expire after 3 seconds so a stray click doesn't leave
    /// the button stuck on "Cancel?".
    pending_cancel_confirm: HashMap<String, Instant>,

    /// True when the previous frame's connection state was `Connected`.
    /// Used in `before_build` to detect the transition out of Connected so
    /// per-session transient UI (context menus, confirmation modals, elevate
    /// prompt, voice-mode dropdown) is reset before it can act on stale ids
    /// from the old server session.
    prev_was_connected: bool,
}

impl<B: UiBackend> RumbleApp<B> {
    pub fn new(backend: B, identity: Identity, settings: SettingsStore, runtime: Runtime) -> Self {
        let wizard = if identity.needs_setup() {
            WizardState::SelectMethod
        } else {
            WizardState::NotNeeded
        };

        // Build the audio-processor registry up front. The schema is
        // also needed by the settings dialog at render time, so the
        // App owns the registry for the lifetime of the process.
        let mut processor_registry = ProcessorRegistry::new();
        register_builtin_processors(&mut processor_registry);

        // Push the initial TX pipeline at boot — either the user's
        // persisted config (merged against the current defaults so
        // newly-added processors slot in), or a fresh default chain.
        let initial_pipeline = match settings.settings().audio.tx_pipeline.as_ref() {
            Some(persisted) => merge_with_default_tx_pipeline(persisted, &processor_registry),
            None => build_default_tx_pipeline(&processor_registry),
        };
        backend.send(Command::UpdateTxPipeline {
            config: initial_pipeline,
        });

        // Push the rest of the persisted audio config so the audio task
        // starts with the user's chosen voice mode, devices, and encoder
        // settings — not the BackendHandle's hardcoded defaults. Without
        // this, a user who last saved Continuous mode boots in PTT every
        // session and the mic stays silent until they reopen Settings.
        let persisted = settings.settings();
        backend.send(Command::SetVoiceMode {
            mode: VoiceMode::from(persisted.voice_mode),
        });
        backend.send(Command::UpdateAudioSettings {
            settings: AudioSettings::from(&persisted.audio),
        });
        backend.send(Command::SetInputDevice {
            device_id: persisted.input_device_id.clone(),
        });
        backend.send(Command::SetOutputDevice {
            device_id: persisted.output_device_id.clone(),
        });

        // Bring up global hotkeys (PTT, mute, deafen) from the user's
        // persisted keyboard bindings. On Wayland the portal backend
        // needs an async init; we use `runtime.block_on` so the
        // initialisation completes before the first frame is built.
        let mut hotkeys = HotkeyManager::new();
        let runtime_handle = runtime.handle().clone();
        runtime.block_on(async {
            hotkeys.init_portal_backend(runtime_handle).await;
        });
        if let Err(e) = hotkeys.register_from_settings(&persisted.keyboard) {
            tracing::warn!("hotkey registration failed: {e}");
        }

        let runtime_handle_for_media_cache = runtime.handle().clone();
        let initial_gif_autoplay = settings.settings().chat.gif_autoplay;
        Self {
            backend,
            identity,
            settings,
            runtime,
            hotkeys,
            wizard,
            unlock: UnlockState::default(),
            elevate: None,
            pending_agent_op: None,
            server_picker: ServerPickerState::default(),
            settings_state: SettingsState::default(),
            voice_mode_menu_open: false,
            force_unlock_for_test: false,
            default_username: default_username(),
            selection: Selection::default(),
            composer: chat::composer::ChatComposerState::default(),
            chat_weights: [1.0, 2.5],
            chat_sidebar_drag: ResizeWeightsDrag::default(),
            chat_row_w: Cell::new(0.0),
            processor_registry,
            room_tree: RoomTreeState::default(),
            seen_chat_ids: None,
            pending_scroll_to_bottom: None,
            sfx: crate::sfx::SfxEngine::default(),
            pending_file_dialog: None,
            auto_handled_offers: HashSet::new(),
            media_cache: crate::media_cache::MediaCache::new(runtime_handle_for_media_cache, initial_gif_autoplay),
            lightboxes: crate::lightbox::Lightboxes::default(),
            file_context_menu: None,
            pending_save_as: None,
            pending_pick_download_dir: None,
            file_drop_hover: false,
            room_acl_modal: None,
            gpu_device: None,
            pending_toasts: Vec::new(),
            pending_link_opens: Vec::new(),
            pending_cancel_confirm: HashMap::new(),
            prev_was_connected: false,
        }
    }
}

fn default_username() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "rumble-user".to_string())
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Compute the subset of `messages` not yet present in `seen`, preserving
/// their order in the slice. Also returns the id set to carry into the next
/// frame (`None` = still unseeded).
///
/// Seeding: while `seen` is `None` and the list is empty, stay unseeded — at
/// that point there is no way to tell whether the next batch will be a
/// connect-time history replay or a live message, and the backlog must not
/// fire sound cues or auto-downloads. The first non-empty observation seeds
/// the set silently (absorbing the initial backlog); every later arrival is
/// diffed by id, which survives the 100-entry sliding window and
/// HistoryMerged re-sorts. Known cost: on a server with no history at all,
/// the first live batch is absorbed as if it were backlog.
fn diff_new_chat_messages<'a>(
    messages: &'a [ChatMessage],
    seen: &Option<HashSet<[u8; 16]>>,
) -> (Vec<&'a ChatMessage>, Option<HashSet<[u8; 16]>>) {
    match seen {
        None if messages.is_empty() => (Vec::new(), None),
        None => (Vec::new(), Some(messages.iter().map(|m| m.id).collect())),
        Some(prev_ids) => {
            let new = messages.iter().filter(|m| !prev_ids.contains(&m.id)).collect();
            (new, Some(messages.iter().map(|m| m.id).collect()))
        }
    }
}

impl<B: UiBackend> Drop for RumbleApp<B> {
    fn drop(&mut self) {
        // Close the XDG GlobalShortcuts portal's D-Bus session while the
        // tokio `runtime` field is still alive. The portal's zbus
        // connection spawns onto the tokio executor as it closes; left to
        // the default field-drop order (`runtime` drops before
        // `hotkeys`), that close runs on the main thread with no reactor
        // in scope and panics with "there is no reactor running". Driving
        // it here keeps the runtime in scope for the teardown.
        self.hotkeys.shutdown_portal(&self.runtime);
    }
}

impl<B: UiBackend> App for RumbleApp<B> {
    fn before_build(&mut self) {
        self.poll_agent_op();
        self.poll_file_dialog();
        self.poll_save_as();
        self.poll_pick_download_dir();
        // Compute new messages once, shared by auto-download and sfx so both
        // operate on the same per-frame diff rather than diffing independently.
        let snapshot = self.backend.state();

        // Detect transition out of Connected. On the frame that sees the
        // drop, clear every piece of per-session transient UI before it has
        // a chance to render: context menus hold room/user ids from the old
        // server; the room-ACL editor and elevate prompt are scoped to the
        // current session; and the voice-mode dropdown is only valid while
        // connected. Cert-pending prompt, server-picker, wizard, settings,
        // chat composer, and toasts are intentionally preserved — they all
        // outlive the session.
        let now_connected = snapshot.connection.is_connected();
        if self.prev_was_connected && !now_connected {
            self.room_tree = RoomTreeState::default();
            self.room_acl_modal = None;
            self.elevate = None;
            self.voice_mode_menu_open = false;
            // Re-arm chat seeding: the next connect clears chat_messages and
            // delivers the new server's history, which must be absorbed as
            // backlog (no sounds/auto-downloads), not detected as new ids.
            self.seen_chat_ids = None;
        }
        self.prev_was_connected = now_connected;

        let was_seeded = self.seen_chat_ids.is_some();
        let (new_messages, new_seen) = diff_new_chat_messages(&snapshot.chat_messages, &self.seen_chat_ids);
        // True only on the frame that absorbs the initial backlog — used to
        // scroll the freshly-loaded history to the bottom without treating
        // it as new arrivals.
        let seeded_now = !was_seeded && new_seen.is_some();
        self.pump_auto_download(&snapshot, &new_messages);
        self.pump_sfx(&snapshot, &new_messages, seeded_now);
        self.seen_chat_ids = new_seen;
        // Drain backend events FIRST so any TransferStageChanged
        // lands on the media cache before its drain_pending +
        // playback advance run for this frame. The order is what
        // makes the new card show up the same frame the transfer
        // finishes (event arrives → media cache decodes synchronously
        // for images → render reads cache).
        self.media_cache
            .set_gif_autoplay_default(self.settings.settings().chat.gif_autoplay);
        self.drain_backend_events();
        self.media_cache.drain_pending();
        let now = Instant::now();
        self.media_cache.advance_gif_playheads(now);
        self.poll_video_open();
        self.pump_hotkeys();
        self.pending_cancel_confirm
            .retain(|_, t| now.duration_since(*t).as_secs() < 3);
        if let Some(active) = self.lightboxes.video.as_mut() {
            active.refresh_scrub_value();
        }
    }

    fn drain_toasts(&mut self) -> Vec<ToastSpec> {
        std::mem::take(&mut self.pending_toasts)
    }

    fn drain_link_opens(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending_link_opens)
    }

    fn drain_scroll_requests(&mut self) -> Vec<ScrollRequest> {
        match self.pending_scroll_to_bottom.take() {
            Some(row_key) => vec![ScrollRequest::to_row_key(
                chat::CHAT_HISTORY_KEY,
                row_key,
                ScrollAlignment::End,
            )],
            None => Vec::new(),
        }
    }

    fn theme(&self) -> Theme {
        Theme::radix_slate_blue_dark()
    }

    fn build(&self, cx: &BuildCx) -> El {
        self.chat_row_w.set(cx.viewport().map(|(w, _)| w).unwrap_or(0.0));
        let state = self.backend.state();
        let shell = self.settings.settings();

        let all_transfers = self.backend.transfers();

        let transfers: chat::TransferMap = all_transfers.iter().map(|t| (t.id.0.clone(), t.clone())).collect();

        let (in_flight_uploads, in_flight_downloads) = {
            use rumble_client_traits::file_transfer::{TransferDirection, TransferStage};
            all_transfers
                .iter()
                .filter(|t| matches!(t.stage, TransferStage::Active { .. } | TransferStage::Paused { .. }))
                .fold((0usize, 0usize), |(up, dn), t| match t.direction {
                    TransferDirection::Upload => (up + 1, dn),
                    TransferDirection::Download => (up, dn + 1),
                })
        };

        let own_username = state
            .my_user_id
            .and_then(|id| state.get_user(id))
            .map(|u| u.username.as_str())
            .unwrap_or("");

        // Toolbar, then an optional device-fault banner (only present when
        // a mic/speaker fault is active), then the chat + center split.
        let mut main_children: Vec<El> = vec![top_toolbar(&state, in_flight_uploads, in_flight_downloads)];
        if let Some(banner) = fault_banner(&state) {
            main_children.push(banner);
        }
        main_children.push(
            row([
                chat::render(
                    &state,
                    &shell.chat,
                    &self.media_cache,
                    &transfers,
                    &self.pending_cancel_confirm,
                    &self.composer.input,
                    &self.selection,
                    self.composer.command_selected,
                    state.my_user_id,
                    own_username,
                )
                .width(Size::Fill(self.chat_weights[0])),
                resize_handle(Axis::Row).key(CHAT_SIDEBAR_HANDLE),
                center_area(&state, &shell.recent_servers, &self.room_tree).width(Size::Fill(self.chat_weights[1])),
            ])
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0))
            .align(Align::Stretch),
        );
        let main = column(main_children).fill_size().align(Align::Stretch);

        // Wizard takes precedence over everything else — until an identity
        // is configured the rest of the UI is read-only.
        let wizard_open = !matches!(self.wizard, WizardState::NotNeeded | WizardState::Complete);
        // First-run wizard (no identity yet) can't be cancelled — the
        // rest of the UI is locked behind having an identity. Once an
        // identity exists (subsequent invocations from Settings), the
        // wizard offers a Cancel out.
        let wizard_cancelable = !self.identity.needs_setup();
        let wizard_layer = wizard::render(
            &self.wizard,
            self.pending_agent_op.is_some(),
            wizard_cancelable,
            &self.selection,
        );

        let unlock_layer = if !wizard_open && (self.identity.needs_unlock() || self.force_unlock_for_test) {
            Some(wizard::render_unlock(&self.unlock, &self.selection))
        } else {
            None
        };

        let cert_layer = if !wizard_open
            && unlock_layer.is_none()
            && let ConnectionState::CertificatePending { cert_info } = &state.connection
        {
            Some(cert_modal(cert_info))
        } else {
            None
        };
        // Session-gate modals (wizard / unlock / cert) lock the whole
        // session; while any is up, lower layers are suppressed.
        let session_gate_clear = !wizard_open && unlock_layer.is_none() && cert_layer.is_none();
        // Content overlays (lightboxes, file context menu) also yield to
        // the settings dialog, which would otherwise paint behind them
        // while still interactive.
        let content_layers_clear = session_gate_clear && !self.settings_state.open;
        // Suppress the server form whenever a higher-priority modal is up.
        let connect_layer = if session_gate_clear {
            server_picker::render_form_modal(&self.server_picker, &self.selection)
        } else {
            None
        };

        let voice_mode_layer = if self.voice_mode_menu_open && session_gate_clear && state.connection.is_connected() {
            Some(voice_mode_menu(state.audio.voice_mode))
        } else {
            None
        };

        let (settings_panel, settings_popover) = if session_gate_clear {
            settings::render(
                &self.settings_state,
                &state,
                &self.backend.meter(),
                &self.backend.stats(),
                &self.backend.outputs(),
                &self.identity,
                &self.selection,
                &self.processor_registry,
                &self.hotkeys,
            )
        } else {
            (None, None)
        };

        let lower_layers_block_room_menus = !content_layers_clear;
        let room_tree_overlays = if lower_layers_block_room_menus {
            room_tree::RoomTreeOverlays::default()
        } else {
            room_tree::render_overlays(&self.room_tree, &state)
        };

        // Lightbox is suppressed whenever a higher-priority modal is
        // up: a settings dialog or wizard would otherwise paint behind
        // the lightbox while still being interactive, which is
        // confusing. Cert/unlock/wizard already gate the chat itself,
        // so anyone with a pending lightbox should have it cleared by
        // those flows — this is just defense in depth.
        //
        // Image source priority: the full-resolution decode is shown
        // when ready; until then we fall back to the chat thumbnail
        // so the panel opens instantly and just gets sharper a frame
        // later.
        let lightbox_layer = if content_layers_clear
            && let Some(lightbox_state) = self.lightboxes.image.as_ref()
            && let Some(cached) = self.media_cache.lightbox_image_for(&lightbox_state.transfer_id)
        {
            let playback = self.media_cache.gif_playback_for(&lightbox_state.transfer_id);
            let gpu = self.media_cache.animated_gpu_for(&lightbox_state.transfer_id);
            // Previous frame's laid-out body rect (one frame stale, fine
            // for the grab-cursor decision); event-time zoom re-queries
            // it live via `EventCx::rect_of_key`.
            let body_size = cx.rect_of_key(chat::KEY_LIGHTBOX_IMAGE).map(|r| (r.w, r.h));
            Some(chat::render_lightbox(lightbox_state, body_size, cached, playback, gpu))
        } else {
            None
        };

        // Video lightbox sits in the same overlay slot as the
        // image lightbox — only one is ever open at a time, so
        // they don't visually conflict. Same modal-suppression
        // rules apply (no popping over wizard / cert / unlock /
        // settings).
        let video_lightbox_layer = if content_layers_clear && let Some(active) = self.lightboxes.video.as_ref() {
            Some(video::render_lightbox(active))
        } else {
            None
        };

        // Model lightbox shares the image/video lightbox overlay slot
        // (only one open at a time) and the same modal-suppression rules.
        let model_lightbox_layer = if content_layers_clear && let Some(active) = self.lightboxes.model.as_ref() {
            Some(model::render_lightbox(active))
        } else {
            None
        };

        let file_ctx_layer = if content_layers_clear {
            self.file_context_menu.as_ref().map(chat::render_file_context_menu)
        } else {
            None
        };

        // Per-room ACL editor modal. Suppressed under cert / unlock /
        // wizard for the same reason settings is — those gate the
        // session itself. Renders above the settings dialog when both
        // are somehow open (shouldn't happen by normal flow).
        let (room_acl_modal_layer, room_acl_popover_layer) =
            if session_gate_clear && let Some(modal) = self.room_acl_modal.as_ref() {
                let layers = room_acl::render(modal, &state, &self.selection);
                (layers.modal, layers.popover)
            } else {
                (None, None)
            };

        // Drop-target hint while the OS is reporting a hovered file.
        // Suppressed under modals (the user can still drop a file then,
        // but the prompt would visually fight whatever modal is up).
        let drop_target_layer = if self.file_drop_hover && session_gate_clear && state.connection.is_connected() {
            Some(drop_target_hint())
        } else {
            None
        };

        // Sudo elevation prompt. Suppressed by the same session-gate
        // modals (wizard/unlock/cert). Renders above settings since the
        // App closes settings when opening it — no overlap by design,
        // but the explicit ordering keeps the precedence obvious.
        let elevate_layer = if session_gate_clear && let Some(es) = self.elevate.as_ref() {
            Some(elevate::render(es, &self.selection))
        } else {
            None
        };

        // Always wrap in `overlays(...)` — even with no app layers, the
        // root must be an `Axis::Overlay` container so damascene's runtime
        // tooltip layer overlays the main view instead of competing for
        // flex space (see vendor/damascene/.../tooltip.rs root precondition).
        //
        // Layer order matters: paints back-to-front. The settings
        // popover sits above its panel; the wizard sits on top of
        // everything because nothing else is allowed to interact
        // while it's open. The lightbox sits above content/menus
        // but below the protective modals (cert/unlock/wizard) so
        // those still take precedence if they appear simultaneously.
        overlays(
            main,
            [
                connect_layer,
                room_tree_overlays.room_context_menu,
                room_tree_overlays.user_context_menu,
                room_tree_overlays.move_room_modal,
                room_tree_overlays.delete_room_modal,
                lightbox_layer,
                video_lightbox_layer,
                model_lightbox_layer,
                file_ctx_layer,
                drop_target_layer,
                voice_mode_layer,
                settings_panel,
                settings_popover,
                room_acl_modal_layer,
                room_acl_popover_layer,
                elevate_layer,
                cert_layer,
                unlock_layer,
                wizard_layer,
            ],
        )
    }

    fn selection(&self) -> Selection {
        self.selection.clone()
    }

    fn on_event(&mut self, event: UiEvent, cx: &EventCx) {
        // The runtime emits `SelectionChanged` when a press / focus move
        // lands somewhere other than a text input — fold it into our
        // single selection slot so static-text + cross-leaf selections
        // clear correctly.
        if event.kind == UiEventKind::SelectionChanged
            && let Some(sel) = event.selection.as_ref()
        {
            self.selection = sel.clone();
            return;
        }

        // Link clicks: the runtime emits `LinkActivated` with the URL in
        // `event.key` whenever a click lands on a text run carrying a
        // `text_link`. Queue it; the host drains the queue and opens
        // each URL through the OS opener.
        if event.kind == UiEventKind::LinkActivated
            && let Some(url) = event.key.as_ref()
        {
            self.pending_link_opens.push(url.clone());
            return;
        }

        // File drag-drop. Routed before modal guards so a drop landing
        // while the wizard / unlock / cert layer is up still reaches
        // the share-file flow — the overlay hint is the only thing
        // suppressed under modals (see drop_target_layer above).
        // Upstream fires one event per file and does NOT auto-cancel
        // the hover on drop, so we clear `file_drop_hover` ourselves.
        match event.kind {
            UiEventKind::FileHovered => {
                self.file_drop_hover = true;
                return;
            }
            UiEventKind::FileHoverCancelled => {
                self.file_drop_hover = false;
                return;
            }
            UiEventKind::FileDropped => {
                self.file_drop_hover = false;
                let Some(path) = event.path.clone() else {
                    return;
                };
                if !self.backend.state().connection.is_connected() {
                    self.backend.send(Command::LocalMessage {
                        text: "Connect to a server before sharing files".to_string(),
                    });
                    return;
                }
                self.backend.send(Command::ShareFile { path });
                return;
            }
            _ => {}
        }

        // Wizard / unlock layers swallow everything until they're done.
        // The wizard scrim is intentionally a no-op (no "click outside
        // to dismiss") so the user can't end up with a half-configured
        // identity by hitting Escape.
        if !matches!(self.wizard, WizardState::NotNeeded | WizardState::Complete) {
            let outcome = wizard::handle_event(&mut self.wizard, &event, &mut self.selection);
            self.dispatch_wizard_outcome(outcome);
            return;
        }
        if self.identity.needs_unlock() {
            let outcome = wizard::handle_unlock_event(&mut self.unlock, &event, &mut self.selection);
            self.dispatch_wizard_outcome(outcome);
            return;
        }

        // Sudo elevation prompt claims its events first so a stray
        // password-field click doesn't reach the chat input behind it.
        if self.elevate.is_some() {
            let outcome = {
                let es = self.elevate.as_mut().expect("checked");
                elevate::handle_event(es, &event, &mut self.selection)
            };
            match outcome {
                ElevateOutcome::Ignored => {}
                ElevateOutcome::Handled => return,
                ElevateOutcome::Cancel => {
                    self.elevate = None;
                    return;
                }
                ElevateOutcome::Submit { password } => {
                    self.backend.send(Command::Elevate { password });
                    self.elevate = None;
                    return;
                }
            }
        }

        // Per-room ACL editor modal claims events before the rest of
        // the UI. Save / Cancel close the modal; everything else
        // mutates pending entries in place.
        if let Some(modal) = self.room_acl_modal.as_mut() {
            let app_state = self.backend.state();
            match room_acl::handle_event(modal, &event, &app_state, &mut self.selection) {
                room_acl::RoomAclOutcome::Ignored => {}
                room_acl::RoomAclOutcome::Handled => return,
                room_acl::RoomAclOutcome::Close => {
                    self.room_acl_modal = None;
                    return;
                }
                room_acl::RoomAclOutcome::Save(cmd) => {
                    self.backend.send(cmd);
                    self.room_acl_modal = None;
                    return;
                }
            }
        }

        // Settings dialog owns its own routed-key namespace; let it
        // claim its events first so the toolbar / chat / room handlers
        // below don't accidentally swallow them.
        if self.settings_state.open {
            let app_state = self.backend.state();
            let outcome = settings::handle_event(
                &mut self.settings_state,
                &event,
                &app_state,
                &self.identity,
                &mut self.selection,
                &self.processor_registry,
                &self.hotkeys,
            );
            if self.dispatch_settings_outcome(outcome) {
                return;
            }
        }

        // Chat sidebar resize. Routed events return early so the
        // handle's drag stream doesn't fall through to other matchers.
        if event.route() == Some(CHAT_SIDEBAR_HANDLE) {
            resize_handle::apply_event_weights(
                &mut self.chat_weights,
                &mut self.chat_sidebar_drag,
                &event,
                CHAT_SIDEBAR_HANDLE,
                Axis::Row,
                self.chat_row_w.get(),
                0.15,
            );
            return;
        }

        // Saved-server picker (list lifecycle + add/edit form).
        match server_picker::handle_event(&mut self.server_picker, &event, &mut self.selection) {
            ServerPickerOutcome::Ignored => {}
            ServerPickerOutcome::Handled => return,
            ServerPickerOutcome::BeginAdd => {
                self.server_picker.begin_add(&self.default_username);
                return;
            }
            ServerPickerOutcome::ConnectRecent(idx) => {
                self.connect_to_recent(idx);
                return;
            }
            ServerPickerOutcome::BeginEdit(idx) => {
                self.open_edit_form(idx);
                return;
            }
            ServerPickerOutcome::DeleteRecent(idx) => {
                self.delete_recent(idx);
                return;
            }
            ServerPickerOutcome::Save => {
                if self.save_server_form().is_some() {
                    self.server_picker.close();
                }
                return;
            }
            ServerPickerOutcome::SaveAndConnect => {
                if let Some(saved) = self.save_server_form() {
                    self.server_picker.close();
                    self.connect_to_server(&saved);
                }
                return;
            }
        }

        // Cert acceptance prompt. The modal is rendered whenever
        // `state.connection` is `CertificatePending`; clicking the scrim
        // is intentionally a no-op so the user has to make an explicit
        // accept/reject decision.
        if event.is_click_or_activate("cert:accept") {
            self.accept_pending_cert();
            return;
        }
        if event.is_click_or_activate("cert:reject") {
            self.backend.send(Command::RejectCertificate);
            return;
        }

        // Chat composer: slash-command navigation/completion, send-on-Enter,
        // Ctrl+V image paste, and plain text editing. The module owns the
        // composer state and returns what the App should do.
        {
            let app_state = self.backend.state();
            match chat::composer::handle_event(&mut self.composer, &mut self.selection, &event, &app_state) {
                chat::composer::ComposerOutcome::Ignored => {}
                chat::composer::ComposerOutcome::Handled => return,
                chat::composer::ComposerOutcome::Send(cmds) => {
                    for cmd in cmds {
                        self.backend.send(cmd);
                    }
                    return;
                }
                chat::composer::ComposerOutcome::PasteImage => {
                    self.paste_clipboard_image();
                    return;
                }
            }
        }
        if event.is_click_or_activate(chat::KEY_PASTE_IMAGE) {
            self.paste_clipboard_image();
            return;
        }
        if event.is_click_or_activate(chat::KEY_SHARE_FILE) {
            self.spawn_share_file_dialog();
            return;
        }
        if event.is_click_or_activate(chat::KEY_SYNC_HISTORY) {
            self.backend.send(Command::RequestChatHistory);
            return;
        }
        // File card right-click → open context menu.
        if event.kind == UiEventKind::SecondaryClick
            && let Some(route) = event.route()
            && let Some(transfer_id) = chat::parse_file_card_key(route)
            && let Some(point) = event.pointer_pos()
        {
            self.open_file_context_menu(transfer_id, point);
            return;
        }

        // File context menu actions.
        if self.file_context_menu.is_some() {
            if event.is_click_or_activate(chat::KEY_FILE_CTX_OPEN) {
                self.file_ctx_open();
                return;
            }
            if event.is_click_or_activate(chat::KEY_FILE_CTX_OPEN_FOLDER) {
                self.file_ctx_open_folder();
                return;
            }
            if event.is_click_or_activate(chat::KEY_FILE_CTX_SAVE_AS) {
                self.file_ctx_save_as();
                return;
            }
            if event.is_route(chat::KEY_FILE_CTX_DISMISS) && event.kind == UiEventKind::Click
                || event.kind == UiEventKind::Escape
            {
                self.file_context_menu = None;
                return;
            }
        }

        if event.kind == UiEventKind::Click
            && let Some(route) = event.route()
            && let Some(transfer_id) = chat::parse_download_key(route)
        {
            self.download_offer(transfer_id);
            return;
        }

        if matches!(event.kind, UiEventKind::Click | UiEventKind::Activate)
            && let Some(route) = event.route()
            && let Some(transfer_id) = chat::parse_cancel_key(route)
        {
            if self.pending_cancel_confirm.contains_key(transfer_id) {
                self.pending_cancel_confirm.remove(transfer_id);
                self.backend.send(Command::CancelTransfer {
                    transfer_id: transfer_id.to_string(),
                });
            } else {
                self.pending_cancel_confirm
                    .insert(transfer_id.to_string(), Instant::now());
            }
            return;
        }

        // Inline Open / Reveal buttons on completed sender and receiver cards.
        if matches!(event.kind, UiEventKind::Click | UiEventKind::Activate)
            && let Some(route) = event.route()
        {
            if let Some(transfer_id) = chat::parse_open_key(route) {
                self.open_transfer_file(transfer_id);
                return;
            }
            if let Some(transfer_id) = chat::parse_reveal_key(route) {
                self.reveal_transfer_file(transfer_id);
                return;
            }
        }

        // Media lightboxes (image / video / 3D model). The module owns
        // the shared overlay state + dispatch; collaborator-heavy opens
        // run here in response to the returned action.
        match lightbox::handle_event(&mut self.lightboxes, &event, cx, &mut self.media_cache) {
            lightbox::LightboxAction::Ignored => {}
            lightbox::LightboxAction::Handled => return,
            lightbox::LightboxAction::OpenImage(id) => {
                self.open_lightbox(&id);
                return;
            }
            lightbox::LightboxAction::OpenVideo(id) => {
                self.open_video_lightbox(&id);
                return;
            }
            lightbox::LightboxAction::OpenModel(id) => {
                self.open_model_lightbox(&id);
                return;
            }
            lightbox::LightboxAction::ToggleGif(id) => {
                self.toggle_gif_playback(&id);
                return;
            }
        }

        // Top toolbar.
        if event.is_click_or_activate("toolbar:mute") {
            let muted = self.backend.state().audio.self_muted;
            self.play_sfx(if muted { SfxKind::Unmute } else { SfxKind::Mute });
            self.backend.send(Command::SetMuted { muted: !muted });
            return;
        }
        if event.is_click_or_activate("toolbar:deafen") {
            let deafened = self.backend.state().audio.self_deafened;
            self.play_sfx(if deafened { SfxKind::Undeafen } else { SfxKind::Deafen });
            self.backend.send(Command::SetDeafened { deafened: !deafened });
            return;
        }
        if let Some(action) = damascene_core::widgets::select::classify_event(&event, KEY_TB_VOICE_MODE) {
            use damascene_core::widgets::select::SelectAction;
            match action {
                SelectAction::Toggle => self.voice_mode_menu_open = !self.voice_mode_menu_open,
                SelectAction::Dismiss => self.voice_mode_menu_open = false,
                SelectAction::Pick(value) => {
                    let next = match value.as_str() {
                        "ptt" => Some(VoiceMode::PushToTalk),
                        "cont" => Some(VoiceMode::Continuous),
                        _ => None,
                    };
                    if let Some(mode) = next {
                        self.backend.send(Command::SetVoiceMode { mode });
                    }
                    self.voice_mode_menu_open = false;
                }
                _ => {}
            }
            return;
        }
        if self.voice_mode_menu_open && event.kind == UiEventKind::Escape {
            self.voice_mode_menu_open = false;
            return;
        }
        if event.is_click_or_activate("toolbar:disconnect") {
            self.backend.send(Command::Disconnect);
            return;
        }
        if event.is_click_or_activate("toolbar:settings") {
            let snapshot = self.backend.state();
            self.settings_state.open_with(&snapshot.audio, self.settings.settings());
            return;
        }

        // Room tree: selection, double-click join, right-click context
        // menus (room + user), drag-and-drop reparenting / self-join,
        // and the confirmation modals those drags fall into. The module
        // owns its own state and returns commands the App fires here.
        let room_tree_state = self.backend.state();
        match room_tree::handle_event(&mut self.room_tree, &event, &room_tree_state, cx) {
            RoomTreeOutcome::Ignored => {}
            RoomTreeOutcome::Handled => (),
            RoomTreeOutcome::Dispatch(commands) => {
                let auto_sync = self.settings.settings().chat.auto_sync_history;
                let has_join = commands.iter().any(|c| matches!(c, Command::JoinRoom { .. }));
                for cmd in &commands {
                    if let Command::JoinRoom { room_id } = cmd {
                        self.sfx.pending_self_join = Some(*room_id);
                    }
                }
                for cmd in commands {
                    // Skip the auto-triggered RequestChatHistory when the
                    // setting is off (manual sync button bypasses this gate).
                    if has_join && !auto_sync && matches!(cmd, Command::RequestChatHistory) {
                        continue;
                    }
                    self.backend.send(cmd);
                }
            }
            RoomTreeOutcome::OpenAclEditor(room_id) => {
                if let Some(modal) = room_acl::RoomAclModalState::open_for(&room_tree_state, room_id) {
                    self.room_acl_modal = Some(modal);
                }
            }
        }
    }

    /// Image lightbox claims the wheel: dy < 0 zooms in, dy > 0 zooms
    /// out. Consumes the event so damascene's default scroll routing
    /// doesn't move the chat list underneath the overlay. Other modes
    /// fall through to the default (forward to `on_event`, no consume),
    /// so chat / room-tree / settings scrolling all keep working.
    fn on_wheel_event(&mut self, event: UiEvent, cx: &EventCx) -> bool {
        if self.lightboxes.image.is_some() {
            let image_size = self.lightboxes.image.as_ref().and_then(|lightbox| {
                let cached = self.media_cache.lightbox_image_for(&lightbox.transfer_id)?;
                let playback = self.media_cache.gif_playback_for(&lightbox.transfer_id);
                Some(cached.current_frame_size(playback))
            });
            let body_size = cx.rect_of_key(chat::KEY_LIGHTBOX_IMAGE).map(|r| (r.w, r.h));
            if let Some(lightbox) = self.lightboxes.image.as_mut()
                && let Some((_, dy)) = event.wheel_delta
            {
                if dy < 0.0 {
                    lightbox.zoom_in(body_size, image_size);
                } else if dy > 0.0 {
                    lightbox.zoom_out(body_size, image_size);
                }
            }
            return true;
        }
        self.on_event(event, cx);
        false
    }
}

impl<B: UiBackend> WinitWgpuApp for RumbleApp<B> {
    /// Stash the wgpu device handle so [`Self::sync_animated_gpu`] can
    /// allocate per-message textures lazily. Both `wgpu::Device` and
    /// `wgpu::Queue` are `Arc`-backed internally — cloning the device
    /// here is cheap.
    fn gpu_setup(&mut self, device: &wgpu::Device, _queue: &wgpu::Queue) {
        self.gpu_device = Some(device.clone());
    }

    /// Per-frame GPU-side update for animated previews: ensures every
    /// active animated entry has a wgpu texture and that the texture
    /// holds the current frame's pixels. CPU-side frame advance has
    /// already happened in [`App::before_build`]
    /// (`pump_gif_animations`); we just mirror the resulting indices
    /// onto the GPU.
    fn before_paint(&mut self, queue: &wgpu::Queue) {
        self.sync_animated_gpu(queue);
        self.sync_active_video_gpu(queue);
        self.sync_model_thumbs(queue);
    }
}

impl<B: UiBackend> RumbleApp<B> {
    /// Persist the currently-pending cert into shared shell settings and
    /// tell the backend to proceed. Dedup by `(server_name, fingerprint)`
    /// so accepting the same cert twice doesn't grow the file.
    fn accept_pending_cert(&mut self) {
        let snapshot = self.backend.state();
        let Some(cert_info) = (match &snapshot.connection {
            ConnectionState::CertificatePending { cert_info } => Some(cert_info.clone()),
            _ => None,
        }) else {
            // Race: state changed between event delivery and now. Send
            // the accept anyway — if there's nothing pending the backend
            // will just warn and ignore it.
            self.backend.send(Command::AcceptCertificate);
            return;
        };
        let server_name = cert_info.server_name.clone();
        let fingerprint = cert_info.fingerprint_hex();
        let der = cert_info.certificate_der.clone();
        self.settings.modify(|s| {
            let already = s
                .accepted_certificates
                .iter()
                .any(|c| c.server_name == server_name && c.fingerprint_hex == fingerprint);
            if !already {
                s.accepted_certificates
                    .push(AcceptedCertificate::from_der(server_name, fingerprint, &der));
            }
        });
        self.backend.send(Command::AcceptCertificate);
    }

    /// Look up `recent_servers[idx]` and dispatch a connect to it.
    /// `idx` is the position in the unsorted Vec — that's the only stable
    /// identifier the row keys carry, so it's safe across re-renders that
    /// re-sort the list visually.
    fn connect_to_recent(&mut self, idx: usize) {
        let Some(server) = self.settings.settings().recent_servers.get(idx).cloned() else {
            tracing::warn!("rumble-damascene: connect_to_recent({idx}) — out of bounds");
            return;
        };
        self.connect_to_server(&server);
    }

    /// Open the form pre-populated with `recent_servers[idx]`. No-op if
    /// idx is out of bounds — the row that sourced the click has gone.
    fn open_edit_form(&mut self, idx: usize) {
        let Some(server) = self.settings.settings().recent_servers.get(idx).cloned() else {
            return;
        };
        self.server_picker.begin_edit(idx, &server);
    }

    fn delete_recent(&mut self, idx: usize) {
        self.settings.modify(|s| {
            if idx < s.recent_servers.len() {
                let removed = s.recent_servers.remove(idx);
                if s.auto_connect_addr.as_deref() == Some(removed.addr.as_str()) {
                    s.auto_connect_addr = None;
                }
            }
        });
    }

    /// Run pre-flight identity checks and dispatch `Command::Connect`
    /// for `server`. Bumps the entry's `last_used_unix` so the list
    /// re-sorts the next frame.
    fn connect_to_server(&mut self, server: &RecentServer) {
        let Some(public_key) = self.identity.public_key() else {
            self.backend.send(Command::LocalMessage {
                text: "Cannot connect: No identity key configured. Please complete first-run setup.".to_string(),
            });
            return;
        };
        if self.identity.needs_unlock() {
            self.backend.send(Command::LocalMessage {
                text: "Cannot connect: Key is encrypted. Please unlock it in settings.".to_string(),
            });
            return;
        }

        let addr = if server.addr.trim().is_empty() {
            "127.0.0.1:5000".to_string()
        } else {
            server.addr.trim().to_string()
        };
        let name = if server.username.trim().is_empty() {
            self.default_username.clone()
        } else {
            server.username.trim().to_string()
        };

        // Mark this entry as the most-recently used so it floats to the
        // top of the list. Done before the connect dispatch so a refused
        // connection still updates the order — matches rumble-egui.
        let bump_addr = addr.clone();
        let bump_name = name.clone();
        self.settings.modify(|s| {
            if let Some(entry) = s.recent_servers.iter_mut().find(|r| r.addr == bump_addr) {
                entry.last_used_unix = now_unix();
                entry.username = bump_name;
            }
        });

        self.backend.send(Command::LocalMessage {
            text: format!("Connecting to {addr}..."),
        });
        self.backend.send(Command::Connect {
            addr,
            name,
            public_key,
            password: None,
        });
    }

    /// Validate + persist the open `ServerForm`. Returns the saved
    /// `RecentServer` on success so callers can chain a connect; returns
    /// `None` (with `form.error` populated) on validation failure.
    ///
    /// Edit semantics: removes the original entry at `editing_index`
    /// first, then writes the new fields keyed by addr. If the new addr
    /// already exists at a different position, that row's label/username
    /// are overwritten and the edit's `last_used_unix` carries over to
    /// the larger of the two.
    fn save_server_form(&mut self) -> Option<RecentServer> {
        let form = self.server_picker.form.as_mut()?;
        let addr = form.addr.trim().to_string();
        if addr.is_empty() {
            form.error = Some("Address is required.".to_string());
            return None;
        }
        let label = form.label.trim().to_string();
        let username = form.username.trim().to_string();
        let editing_index = form.editing_index;

        let mut saved: Option<RecentServer> = None;
        self.settings.modify(|s| {
            let preserved_last_used = editing_index
                .and_then(|idx| s.recent_servers.get(idx))
                .map(|r| r.last_used_unix)
                .unwrap_or(0);
            if let Some(idx) = editing_index
                && idx < s.recent_servers.len()
            {
                let original_addr = s.recent_servers[idx].addr.clone();
                s.recent_servers.remove(idx);
                if s.auto_connect_addr.as_deref() == Some(original_addr.as_str()) {
                    // Keep auto-connect pointing at this entry by
                    // updating the addr below.
                    s.auto_connect_addr = Some(addr.clone());
                }
            }
            let entry = if let Some(existing) = s.recent_servers.iter_mut().find(|r| r.addr == addr) {
                existing.label = label.clone();
                existing.username = username.clone();
                existing.last_used_unix = existing.last_used_unix.max(preserved_last_used);
                existing.clone()
            } else {
                let new_entry = RecentServer {
                    addr: addr.clone(),
                    label: label.clone(),
                    username: username.clone(),
                    last_used_unix: preserved_last_used,
                };
                s.recent_servers.push(new_entry.clone());
                new_entry
            };
            saved = Some(entry);
        });
        // Carry the just-used username forward as the default for the
        // next add-server flow. Per-server usernames are still the
        // authoritative value at connect time; this only seeds new forms.
        if let Some(saved) = saved.as_ref()
            && !saved.username.is_empty()
        {
            self.default_username = saved.username.clone();
        }
        saved
    }

    // ---------- chat ----------

    /// Spawn the OS file picker on `runtime`. The result is consumed
    /// by [`Self::poll_file_dialog`] on the next frame so we don't
    /// block the render loop while the (potentially slow) portal
    /// dialog is open.
    fn spawn_share_file_dialog(&mut self) {
        if !self.backend.state().connection.is_connected() {
            self.backend.send(Command::LocalMessage {
                text: "Connect to a server before sharing files".to_string(),
            });
            return;
        }
        if self.pending_file_dialog.is_some() {
            // Already showing — ignore the click rather than stack
            // multiple dialogs on top of each other.
            return;
        }
        let wake = self.backend.repaint_arc();
        let handle = self.runtime.spawn(async move {
            let result = rfd::AsyncFileDialog::new()
                .pick_file()
                .await
                .map(|f| f.path().to_path_buf());
            // Wake the frame scheduler so poll_file_dialog runs
            // immediately rather than stalling until user input.
            wake();
            result
        });
        self.pending_file_dialog = Some(handle);
    }

    /// Push `text` onto the OS clipboard and surface either `success_msg`
    /// or a generic failure as a local chat line. `arboard` opens a
    /// fresh handle per call — short-lived and side-effect-free, same
    /// pattern as `paste_clipboard_image`.
    fn copy_to_clipboard(&mut self, text: String, success_msg: &str) {
        match arboard::Clipboard::new().and_then(|mut c| c.set_text(text)) {
            Ok(()) => {
                self.backend.send(Command::LocalMessage {
                    text: success_msg.to_string(),
                });
            }
            Err(e) => {
                tracing::warn!("clipboard write failed: {e}");
                self.backend.send(Command::LocalMessage {
                    text: "Could not write to the clipboard".to_string(),
                });
            }
        }
    }

    /// Read an image off the system clipboard, write it to a temp PNG,
    /// and dispatch a `ShareFile` for it. Mirrors the rumble-egui
    /// helper of the same name; arboard works independently of any
    /// runtime clipboard plumbing so this runs even if winit's text
    /// clipboard isn't wired (e.g. egui#2108 on the egui side).
    fn paste_clipboard_image(&mut self) {
        if !self.backend.state().connection.is_connected() {
            self.backend.send(Command::LocalMessage {
                text: "Connect to a server before pasting images".to_string(),
            });
            return;
        }
        let mut clipboard = match arboard::Clipboard::new() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("clipboard open failed: {e}");
                self.backend.send(Command::LocalMessage {
                    text: "Could not access the clipboard".to_string(),
                });
                return;
            }
        };
        let img_data = match clipboard.get_image() {
            Ok(d) => d,
            Err(_) => {
                self.backend.send(Command::LocalMessage {
                    text: "No image on clipboard".to_string(),
                });
                return;
            }
        };
        let Some(rgba) = image::RgbaImage::from_raw(
            img_data.width as u32,
            img_data.height as u32,
            img_data.bytes.into_owned(),
        ) else {
            self.backend.send(Command::LocalMessage {
                text: "Failed to process clipboard image".to_string(),
            });
            return;
        };
        let temp_dir = match tempfile::tempdir() {
            Ok(d) => d,
            Err(e) => {
                tracing::error!("tempdir create failed: {e}");
                return;
            }
        };
        let temp_path = temp_dir.path().join("clipboard_image.png");
        if let Err(e) = rgba.save(&temp_path) {
            tracing::error!("clipboard image save failed: {e}");
            self.backend.send(Command::LocalMessage {
                text: "Failed to write clipboard image to a temp file".to_string(),
            });
            return;
        }
        // Fast-fail oversized PNGs before dispatching ShareFile. The
        // backend re-validates and is the source of truth; this just
        // surfaces the error a beat sooner and avoids a spurious
        // pending card flash.
        match std::fs::metadata(&temp_path) {
            Ok(md) if md.len() > rumble_client_traits::MAX_UPLOAD_BYTES => {
                self.pending_toasts.push(damascene_core::toast::ToastSpec::new(
                    damascene_core::toast::ToastLevel::Error,
                    "Pasted image is too large to share".to_string(),
                ));
                return;
            }
            _ => {}
        }
        // Keep the temp dir alive for the lifetime of the share —
        // ShareFile copies the bytes into the relay stream, but the
        // file must still exist when the relay task picks it up.
        // Process exit cleans the OS temp dir; an explicit cleanup
        // belongs with the broader transfer-history work.
        std::mem::forget(temp_dir);
        self.backend.send(Command::ShareFile { path: temp_path });
    }

    /// Spawn the OS folder picker for the download-location setting.
    /// The picker resolves through [`Self::poll_pick_download_dir`],
    /// which pokes the result into the still-open settings dialog.
    fn spawn_pick_download_dir(&mut self) {
        if self.pending_pick_download_dir.is_some() {
            return;
        }
        let initial = self
            .settings_state
            .pending
            .as_ref()
            .and_then(|p| p.download_dir.clone())
            .or_else(|| self.settings.settings().file_transfer.download_dir.clone());
        let wake = self.backend.repaint_arc();
        let handle = self.runtime.spawn(async move {
            let mut dialog = rfd::AsyncFileDialog::new();
            if let Some(start) = initial {
                dialog = dialog.set_directory(start);
            }
            let result = dialog.pick_folder().await.map(|f| f.path().to_path_buf());
            wake();
            result
        });
        self.pending_pick_download_dir = Some(handle);
    }

    /// Drain the folder-picker task and update the settings dialog's
    /// pending download directory. Cancel (None) leaves the value
    /// unchanged.
    fn poll_pick_download_dir(&mut self) {
        let Some(handle) = self.pending_pick_download_dir.as_ref() else {
            return;
        };
        if !handle.is_finished() {
            return;
        }
        let handle = self.pending_pick_download_dir.take().unwrap();
        match self.runtime.block_on(handle) {
            Ok(Some(path)) => self.settings_state.set_pending_download_dir(Some(path)),
            Ok(None) => {}
            Err(e) => tracing::error!("download-dir picker panicked: {e}"),
        }
    }

    /// Drain a finished file-picker task, dispatching `Command::ShareFile`
    /// when the user picked a path. Cancel (None) is silent.
    fn poll_file_dialog(&mut self) {
        let Some(handle) = self.pending_file_dialog.as_ref() else {
            return;
        };
        if !handle.is_finished() {
            return;
        }
        let handle = self.pending_file_dialog.take().unwrap();
        match self.runtime.block_on(handle) {
            Ok(Some(path)) => {
                self.backend.send(Command::ShareFile { path });
            }
            Ok(None) => {
                // User dismissed the picker.
            }
            Err(e) => {
                tracing::error!("file dialog task panicked: {e}");
                self.backend.send(Command::LocalMessage {
                    text: "File picker failed — see logs".to_string(),
                });
            }
        }
    }

    // ---------- file context menu ----------

    /// Open the right-click context menu for a file card. Looks up the
    /// transfer's local_path so menu items can be enabled/disabled.
    fn open_file_context_menu(&mut self, transfer_id: &str, point: (f32, f32)) {
        // Find name and local_path from the offer in chat history.
        let snapshot = self.backend.state();
        let offer = snapshot.chat_messages.iter().find_map(|m| {
            let o = chat::relay_payload(m)?;
            (o.transfer_id == transfer_id).then_some(o)
        });
        let name = offer.map(|o| o.name).unwrap_or_else(|| transfer_id.to_string());
        let local_path = self
            .backend
            .transfers()
            .into_iter()
            .find(|t| t.id.0 == transfer_id)
            .and_then(|t| t.done_path().cloned());
        self.file_context_menu = Some(chat::FileContextMenu {
            transfer_id: transfer_id.to_string(),
            name,
            local_path,
            point,
        });
    }

    /// Open the file with the OS default application.
    fn file_ctx_open(&mut self) {
        if let Some(menu) = self.file_context_menu.take()
            && let Some(path) = &menu.local_path
        {
            open_path(path);
        }
    }

    /// Open the folder containing the downloaded file.
    fn file_ctx_open_folder(&mut self) {
        if let Some(menu) = self.file_context_menu.take()
            && let Some(path) = &menu.local_path
        {
            let folder = path.parent().unwrap_or(path);
            open_path(folder);
        }
    }

    /// Open a completed transfer file by transfer_id using the OS default application.
    fn open_transfer_file(&mut self, transfer_id: &str) {
        if let Some(path) = self
            .backend
            .transfers()
            .into_iter()
            .find(|t| t.id.0 == transfer_id)
            .and_then(|t| t.done_path().cloned())
        {
            open_path(&path);
        }
    }

    /// Open the folder containing a completed transfer file.
    fn reveal_transfer_file(&mut self, transfer_id: &str) {
        if let Some(path) = self
            .backend
            .transfers()
            .into_iter()
            .find(|t| t.id.0 == transfer_id)
            .and_then(|t| t.done_path().cloned())
        {
            let folder = path.parent().unwrap_or(&path);
            open_path(folder);
        }
    }

    /// Spawn a "Save As" dialog. The copy is performed when the dialog
    /// resolves via [`Self::poll_save_as`].
    fn file_ctx_save_as(&mut self) {
        let Some(menu) = self.file_context_menu.take() else {
            return;
        };
        let Some(src) = menu.local_path else { return };
        if self.pending_save_as.is_some() {
            return;
        }
        let name = menu.name.clone();
        let wake = self.backend.repaint_arc();
        let handle = self.runtime.spawn(async move {
            let result = rfd::AsyncFileDialog::new()
                .set_file_name(&name)
                .save_file()
                .await
                .map(|f| f.path().to_path_buf());
            wake();
            result
        });
        self.pending_save_as = Some((src, handle));
    }

    /// Poll the in-flight "Save As" dialog and copy the file when done.
    fn poll_save_as(&mut self) {
        let Some((_, handle)) = self.pending_save_as.as_ref() else {
            return;
        };
        if !handle.is_finished() {
            return;
        }
        let (src, handle) = self.pending_save_as.take().unwrap();
        match self.runtime.block_on(handle) {
            Ok(Some(dest)) => {
                if !src.exists() {
                    tracing::error!("Save As: source file no longer exists: {}", src.display());
                    self.pending_toasts.push(damascene_core::toast::ToastSpec::new(
                        damascene_core::toast::ToastLevel::Error,
                        "Source file no longer exists".to_string(),
                    ));
                    return;
                }
                if let Err(e) = std::fs::copy(&src, &dest) {
                    tracing::error!("Save As copy failed: {e}");
                    self.backend.send(Command::LocalMessage {
                        text: format!("Save As failed: {e}"),
                    });
                }
            }
            Ok(None) => {}
            Err(e) => tracing::error!("Save As dialog panicked: {e}"),
        }
    }

    /// Look up the offer matching `transfer_id` in the visible chat
    /// history and dispatch `Command::DownloadFile` with its
    /// `share_data`. Track the id in `auto_handled_offers` so a
    /// history-replay doesn't re-prompt or re-trigger the download.
    fn download_offer(&mut self, transfer_id: &str) {
        let snapshot = self.backend.state();
        let offer = snapshot.chat_messages.iter().rev().find_map(|m| {
            let o = chat::relay_payload(m)?;
            (o.transfer_id == transfer_id).then_some(o)
        });
        let Some(offer) = offer else {
            tracing::warn!("download_offer: offer {transfer_id} no longer in history");
            return;
        };
        self.auto_handled_offers.insert(offer.transfer_id.clone());
        self.backend.send(Command::DownloadFile {
            share_data: offer.share_data,
        });
    }

    /// Open the click-to-enlarge lightbox for the chat message whose
    /// `FileOffer` matches `transfer_id`. No-op if the offer is no
    /// longer in history (chat truncated mid-frame) or if the image
    /// hasn't been decoded yet — the latter shouldn't happen because
    /// the preview only renders once the cache entry exists.
    ///
    /// Spawns a full-resolution decode of the underlying file on the
    /// app's runtime so the lightbox can show the source at native
    /// quality once it lands. Until then the panel renders the same
    /// 1024-capped thumbnail as the inline preview, so the open is
    /// always immediate.
    fn open_lightbox(&mut self, transfer_id: &str) {
        if self.media_cache.image_for(transfer_id).is_none() {
            return;
        }
        let snapshot = self.backend.state();
        let name = snapshot
            .chat_messages
            .iter()
            .rev()
            .find_map(|m| {
                let o = chat::relay_payload(m)?;
                (o.transfer_id == transfer_id).then_some(o.name)
            })
            .unwrap_or_else(|| transfer_id.to_string());
        // The three lightboxes share one overlay slot — close the others.
        self.close_video_lightbox();
        self.close_model_lightbox();
        self.lightboxes.image = Some(chat::Lightbox::new(transfer_id, name));
        self.media_cache.force_resume_playback(transfer_id);

        // Look up the local file path from the live transfer set; if
        // it's missing (e.g., the transfer plugin GC'd a finished
        // entry) the lightbox just keeps showing the thumbnail.
        let path = self
            .backend
            .transfers()
            .into_iter()
            .find(|t| t.id.0 == transfer_id)
            .and_then(|t| t.done_path().cloned());
        let Some(path) = path else {
            return;
        };
        self.media_cache.open_lightbox_decode(transfer_id, path);
    }

    /// Close the lightbox: clear the active state, drop the
    /// full-resolution image to free GPU memory, and abort any
    /// in-flight decode so it can't deliver a result we'd just throw
    /// away.
    fn close_lightbox(&mut self) {
        self.lightboxes.close_image(&mut self.media_cache);
    }

    /// Open the video lightbox for `transfer_id`. Looks up the
    /// downloaded file's local path from the live transfer set and
    /// kicks off `VideoStream::open` on the runtime — `load_file`
    /// blocks 10–50ms while libmpv parses the container, which
    /// would hitch the UI if done synchronously. The completed
    /// stream lands in `self.lightboxes.video` next frame via
    /// [`Self::poll_video_open`].
    ///
    /// Closing any previous video lightbox (or aborting an
    /// in-flight open) is the caller's responsibility-of-state
    /// here: we drop both before kicking off the new open so a
    /// rapid re-click doesn't leak a libmpv handle.
    fn open_video_lightbox(&mut self, transfer_id: &str) {
        // Re-clicking the same Play button while the lightbox is
        // already open is a no-op — avoids dropping the running
        // libmpv handle and re-opening from scratch when a
        // stale Activate (e.g. focus left on the chat-side
        // preview, user hits Space) sends us the same id.
        if self.lightboxes.video.as_ref().map(|a| a.transfer_id.as_str()) == Some(transfer_id) {
            return;
        }
        // Find the file's name + path from the live transfer set.
        // Bail if the transfer was GC'd between the click and the
        // event firing.
        let entry = self.backend.transfers().into_iter().find(|t| t.id.0 == transfer_id);
        let Some(status) = entry else {
            return;
        };
        let Some(path) = status.done_path().cloned() else {
            return;
        };
        let name = status.name.clone();
        let id = status.id.0.clone();

        // Drop any existing active video / pending open. Newer
        // intent supersedes older. Close the image/model lightboxes too —
        // the three share one overlay slot.
        self.lightboxes.video = None;
        self.close_lightbox();
        self.close_model_lightbox();
        if let Some(prev) = self.lightboxes.pending_video_open.take() {
            prev.abort();
        }

        let wake = self.backend.repaint_arc();
        self.lightboxes.pending_video_open = Some(self.runtime.spawn_blocking(move || {
            // Open looped so the lightbox doesn't freeze the
            // moment a short clip ends — matches the "preview"
            // expectation users have for chat-attachment video.
            let result = rumble_video::VideoStream::open(&path, true).map(|stream| (id, name, stream));
            // Wake on both success and error so poll_video_open runs
            // promptly without requiring user input.
            wake();
            result
        }));
    }

    /// Tear down the video lightbox: drop the stream (which
    /// joins the worker and terminates libmpv), drop the GPU
    /// mirror, abort any in-flight open.
    fn close_video_lightbox(&mut self) {
        self.lightboxes.close_video();
    }

    /// Open the orbit lightbox for a downloaded 3D model. Clones the
    /// parsed geometry out of the media cache (cheap `Arc` bump) into
    /// `active_model`; the lightbox rebuilds its `chart3d` scene from it
    /// each frame. No-op if the model hasn't finished parsing, or if it's
    /// already the open model (a stale re-activate shouldn't reset the
    /// orbit camera). Closes any image/video lightbox first — the three
    /// share one overlay slot.
    fn open_model_lightbox(&mut self, transfer_id: &str) {
        if self.lightboxes.model.as_ref().map(|a| a.transfer_id.as_str()) == Some(transfer_id) {
            return;
        }
        let Some(loaded) = self.media_cache.model_for(transfer_id) else {
            return;
        };
        let model = loaded.clone();
        let name = self
            .backend
            .transfers()
            .into_iter()
            .find(|t| t.id.0 == transfer_id)
            .map(|t| t.name)
            .unwrap_or_else(|| transfer_id.to_string());
        self.close_lightbox();
        self.close_video_lightbox();
        self.lightboxes.model = Some(model::ActiveModel::new(transfer_id, name, model));
    }

    /// Close the model lightbox.
    fn close_model_lightbox(&mut self) {
        self.lightboxes.close_model();
    }

    /// Drain a finished `VideoStream::open` task. On success,
    /// promote the stream to `active_video`; on failure, log and
    /// drop. The GPU mirror is allocated lazily in
    /// [`Self::sync_active_video_gpu`] once the host's wgpu
    /// device is available.
    fn poll_video_open(&mut self) {
        let Some(handle) = self.lightboxes.pending_video_open.as_ref() else {
            return;
        };
        if !handle.is_finished() {
            return;
        }
        let handle = self.lightboxes.pending_video_open.take().unwrap();
        let result = match self.runtime.block_on(handle) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "video: open task panicked");
                return;
            }
        };
        match result {
            Ok((id, name, stream)) => {
                self.lightboxes.video = Some(video::ActiveVideo::new(id, name, stream));
            }
            Err(e) => {
                tracing::warn!(error = %e, "video: stream open failed");
            }
        }
    }

    /// Flip play/pause on an animated entry's playhead. No-op for static
    /// entries (their `gif_playback` slot doesn't exist) — the icon
    /// overlay is only rendered for animated cache entries, so this is
    /// reachable defensively at most.
    fn toggle_gif_playback(&mut self, transfer_id: &str) {
        // MediaCache resets the wall-clock anchor on every toggle so
        // the current frame still shows for its full delay rather than
        // immediately advancing if the entry sat paused for a while.
        let _ = self.media_cache.toggle_playback(transfer_id);
    }

    /// Scan newly-arrived chat messages and auto-download any `FileOffer`
    /// attachments that pass the user's auto-download rules. Receives the
    /// per-frame new-message slice already computed in `before_build`; an
    /// empty slice (seeding frame or no arrivals) is a no-op.
    fn pump_auto_download(&mut self, snapshot: &State, new_messages: &[&ChatMessage]) {
        if new_messages.is_empty() {
            return;
        }
        let settings = self.settings.settings().file_transfer.clone();
        let my_user_id = snapshot.my_user_id;
        // Username fallback for messages from older peers/servers that
        // don't populate `sender_id`. Unreliable when two clients share
        // a username (e.g. same-machine `$USER`), so `sender_id` is
        // preferred whenever it's present.
        let my_username = my_user_id
            .and_then(|id| snapshot.get_user(id))
            .map(|u| u.username.clone());

        for msg in new_messages {
            // System notices have no payload; SenderMirror twins are own
            // shares already accounted for by their SenderDraft. The
            // is_from_self check below further excludes SenderDrafts.
            if matches!(
                msg.visibility,
                rumble_client::ChatMessageVisibility::System | rumble_client::ChatMessageVisibility::SenderMirror
            ) {
                continue;
            }
            let Some(offer) = chat::relay_payload(msg) else {
                continue;
            };
            let is_from_self = match (my_user_id, msg.sender_id) {
                (Some(mine), Some(theirs)) => mine == theirs,
                _ => my_username.as_deref() == Some(msg.sender.as_str()),
            };
            if is_from_self {
                continue;
            }
            // Idempotency guard for mid-session history replays.
            if !self.auto_handled_offers.insert(offer.transfer_id.clone()) {
                continue;
            }
            if settings.should_auto_download(&offer.mime, offer.size) {
                tracing::info!(
                    "auto-download: accepting offer {} ({} bytes, mime={})",
                    offer.name,
                    offer.size,
                    offer.mime
                );
                self.backend.send(Command::DownloadFile {
                    share_data: offer.share_data,
                });
            }
        }
    }

    /// Fire per-frame event sounds for new chat messages, connect/disconnect
    /// transitions, and remote users entering/leaving our room. Receives the
    /// per-frame new-message slice already computed in `before_build`; the
    /// seeding frame passes an empty slice (with `seeding` set) so the
    /// historical backlog on first connect doesn't beep.
    fn pump_sfx(&mut self, snapshot: &State, new_messages: &[&ChatMessage], seeding: bool) {
        // Autoscroll: stick to the newest renderable row whenever any
        // genuinely-new renderable message arrived — and on the seeding
        // frame, so the initial history load lands scrolled to the bottom
        // even though it produces no "new" messages. The target is the last
        // renderable row across the full snapshot — virtual lists don't
        // auto-pin, so we drive it explicitly.
        if (seeding || new_messages.iter().any(|m| m.visibility.renders_locally()))
            && let Some(last) = snapshot
                .chat_messages
                .iter()
                .rev()
                .find(|m| m.visibility.renders_locally())
        {
            self.pending_scroll_to_bottom = Some(chat::chat_row_key(last));
        }

        let sounds = self.sfx.detect(snapshot, new_messages);
        for kind in sounds {
            self.play_sfx(kind);
        }

        // Drop accepted-offer ids when the connection drops so a
        // fresh session re-runs auto-download evaluation.
        if !snapshot.connection.is_connected() && !self.auto_handled_offers.is_empty() {
            self.auto_handled_offers.clear();
        }
    }

    fn play_sfx(&self, kind: SfxKind) {
        let sfx = &self.settings.settings().sfx;
        if !sfx.is_kind_enabled(kind) || sfx.volume <= 0.0 {
            return;
        }
        self.backend.send(Command::PlaySfx {
            kind,
            volume: sfx.volume.clamp(0.0, 1.0),
        });
    }

    /// Drain queued global hotkey events and turn them into backend
    /// commands. PTT (Hold) fires on Pressed / Released; Mute / Deafen
    /// (Toggle / On / Off) fire on Pressed only.
    ///
    /// All hotkeys are gated on `connection.is_connected()` — toggling
    /// state while disconnected would be confusing and produce stray
    /// SetMuted / SetDeafened sends that the server would never see.
    /// Server-muted users have PTT suppressed (the server would drop the
    /// packets anyway).
    fn pump_hotkeys(&mut self) {
        use rumble_desktop_shell::{HotkeyData, HotkeyFunction};
        let events = self.hotkeys.poll_events();
        if events.is_empty() {
            return;
        }
        let state = self.backend.state();
        if !state.connection.is_connected() {
            return;
        }
        let server_muted = state
            .my_user_id
            .and_then(|id| state.get_user(id))
            .is_some_and(|u| u.server_muted);
        for event in events {
            match event {
                HotkeyEvent::Pressed {
                    function: HotkeyFunction::PushToTalk,
                    data: HotkeyData::Hold,
                } => {
                    if server_muted {
                        continue;
                    }
                    self.backend.send(Command::StartTransmit);
                }
                HotkeyEvent::Released {
                    function: HotkeyFunction::PushToTalk,
                    data: HotkeyData::Hold,
                } => {
                    self.backend.send(Command::StopTransmit);
                }
                HotkeyEvent::Pressed {
                    function: HotkeyFunction::MuteSelf,
                    data,
                } => {
                    let muted = match data {
                        HotkeyData::Toggle => !state.audio.self_muted,
                        HotkeyData::On => true,
                        HotkeyData::Off => false,
                        HotkeyData::Hold => continue,
                    };
                    if muted != state.audio.self_muted {
                        self.play_sfx(if muted { SfxKind::Mute } else { SfxKind::Unmute });
                    }
                    self.backend.send(Command::SetMuted { muted });
                }
                HotkeyEvent::Pressed {
                    function: HotkeyFunction::DeafenSelf,
                    data,
                } => {
                    let deafened = match data {
                        HotkeyData::Toggle => !state.audio.self_deafened,
                        HotkeyData::On => true,
                        HotkeyData::Off => false,
                        HotkeyData::Hold => continue,
                    };
                    if deafened != state.audio.self_deafened {
                        self.play_sfx(if deafened { SfxKind::Deafen } else { SfxKind::Undeafen });
                    }
                    self.backend.send(Command::SetDeafened { deafened });
                }
                HotkeyEvent::Pressed { .. } | HotkeyEvent::Released { .. } => {
                    // Nonsensical combinations (e.g. PTT/Toggle) are
                    // filtered at the UI layer; ignore any that slip
                    // through. Non-Hold actions also ignore release.
                }
            }
        }
    }

    /// Drain pending [`rumble_client::BackendEvent`]s and convert them
    /// into [`ToastSpec`]s buffered in `pending_toasts`. The damascene
    /// runtime picks these up via [`App::drain_toasts`] and synthesizes
    /// the toast overlay layer automatically.
    fn drain_backend_events(&mut self) {
        use damascene_core::toast::ToastLevel;
        use rumble_client::NotificationLevel;
        for event in self.backend.drain_events() {
            // Always offer the event to media_cache first — it
            // ignores variants it doesn't care about. The match
            // below only handles UI-side reactions (toast queue);
            // the actual transfer-state handling lives in
            // media_cache::on_event.
            self.media_cache.on_event(&event);
            match event {
                rumble_client::BackendEvent::Toast { level, text } => {
                    let tl = match level {
                        NotificationLevel::Info => ToastLevel::Info,
                        NotificationLevel::Warn => ToastLevel::Warning,
                        NotificationLevel::Error => ToastLevel::Error,
                    };
                    self.pending_toasts.push(ToastSpec::new(tl, text));
                }
                rumble_client::BackendEvent::TransferStageChanged { .. } => {
                    // Media cache already handled it above; no
                    // additional UI-side reaction needed here.
                }
            }
        }
    }

    /// Sync GPU mirrors for animated entries: delegates to
    /// `media_cache.sync_animated_gpu`. Called from
    /// `WinitWgpuApp::before_paint` once the wgpu device is available
    /// — `gpu_setup` stashes it on App.
    fn sync_animated_gpu(&mut self, queue: &wgpu::Queue) {
        let Some(device) = self.gpu_device.as_ref() else {
            return;
        };
        self.media_cache.sync_animated_gpu(device, queue);
    }

    /// Lazily allocate the active video's GPU mirror (needs the
    /// wgpu device, which arrives after the stream itself), then
    /// upload the latest decoded frame if the worker has produced
    /// one since last paint.
    fn sync_active_video_gpu(&mut self, queue: &wgpu::Queue) {
        let Some(device) = self.gpu_device.as_ref() else {
            return;
        };
        let Some(active) = self.lightboxes.video.as_mut() else {
            return;
        };
        if active.gpu.is_none() {
            active.gpu = Some(rumble_video::VideoGpu::allocate(device, &active.stream));
        }
        if let Some(gpu) = active.gpu.as_mut() {
            gpu.upload_if_changed(queue, &active.stream);
        }
    }

    /// Render poster thumbnails for any newly-parsed 3D models. Delegates
    /// to `media_cache.sync_model_thumbs`, which needs the wgpu device for
    /// its offscreen render — `gpu_setup` stashes it on App.
    fn sync_model_thumbs(&mut self, queue: &wgpu::Queue) {
        let Some(device) = self.gpu_device.as_ref() else {
            return;
        };
        self.media_cache.sync_model_thumbs(device, queue);
    }

    // ---------- wizard plumbing ----------

    fn dispatch_wizard_outcome(&mut self, outcome: WizardOutcome) {
        match outcome {
            WizardOutcome::Ignored | WizardOutcome::Handled => {}
            WizardOutcome::Cancel => {
                self.wizard = WizardState::NotNeeded;
            }
            WizardOutcome::SpawnConnect => {
                self.spawn_connect_op();
            }
            WizardOutcome::SpawnAddKey { comment } => {
                self.spawn_add_key_op(comment);
            }
            WizardOutcome::GenerateLocal { password } => {
                if let Some(info) = wizard::apply_generate_local(&mut self.wizard, &mut self.identity, password) {
                    self.notify_identity_ready(format!("Identity key generated: {}", info.fingerprint));
                }
            }
            WizardOutcome::SelectAgentKey { key_info } => {
                if let Some(info) = wizard::apply_select_agent_key(&mut self.wizard, &mut self.identity, &key_info) {
                    self.notify_identity_ready(format!("Using SSH agent key: {} ({})", info.comment, info.fingerprint));
                }
            }
            WizardOutcome::Unlock { password } => {
                if wizard::apply_unlock(&mut self.unlock, &mut self.identity) {
                    let _ = password;
                    self.backend.send(Command::LocalMessage {
                        text: "Identity unlocked.".to_string(),
                    });
                }
            }
        }
    }

    fn spawn_connect_op(&mut self) {
        if self.pending_agent_op.is_some() {
            return;
        }
        let handle = self.runtime.spawn(connect_and_list_keys());
        self.pending_agent_op = Some(PendingAgentOp::Connect(handle));
    }

    fn spawn_add_key_op(&mut self, comment: String) {
        if self.pending_agent_op.is_some() {
            return;
        }
        let handle = self.runtime.spawn(generate_and_add_to_agent(comment));
        self.pending_agent_op = Some(PendingAgentOp::AddKey(handle));
    }

    /// Drain a finished agent op, advancing wizard state with the result.
    /// Called from `before_build` so the new state is visible on the
    /// next frame.
    fn poll_agent_op(&mut self) {
        let Some(op) = self.pending_agent_op.as_ref() else {
            return;
        };
        let finished = match op {
            PendingAgentOp::Connect(h) => h.is_finished(),
            PendingAgentOp::AddKey(h) => h.is_finished(),
        };
        if !finished {
            return;
        }
        match self.pending_agent_op.take().unwrap() {
            PendingAgentOp::Connect(handle) => match self.runtime.block_on(handle) {
                Ok(Ok(keys)) => {
                    self.wizard = WizardState::SelectAgentKey {
                        keys,
                        selected: None,
                        error: None,
                    };
                }
                Ok(Err(e)) => {
                    self.wizard = WizardState::Error {
                        message: format!("Failed to connect to SSH agent: {e}"),
                    };
                }
                Err(e) => {
                    self.wizard = WizardState::Error {
                        message: format!("Agent operation panicked: {e}"),
                    };
                }
            },
            PendingAgentOp::AddKey(handle) => match self.runtime.block_on(handle) {
                Ok(Ok(key_info)) => {
                    if let Some(info) = wizard::apply_select_agent_key(&mut self.wizard, &mut self.identity, &key_info)
                    {
                        self.notify_identity_ready(format!(
                            "Added new SSH agent key: {} ({})",
                            info.comment, info.fingerprint
                        ));
                    }
                }
                Ok(Err(e)) => {
                    self.wizard = WizardState::Error {
                        message: format!("Failed to add key to agent: {e}"),
                    };
                }
                Err(e) => {
                    self.wizard = WizardState::Error {
                        message: format!("Agent operation panicked: {e}"),
                    };
                }
            },
        }
    }

    fn notify_identity_ready(&self, msg: String) {
        self.backend.send(Command::LocalMessage { text: msg });
    }

    /// Re-register hotkeys from the persisted store. Called whenever
    /// Settings is closed without saving so live-preview binding changes
    /// (applied incrementally via [`SettingsOutcome::RegisterHotkeys`] on
    /// each edit) do not outlast the dialog — cancelled edits to PTT
    /// rows, rebindings, and the enable toggle are all rolled back.
    fn restore_hotkeys_from_persisted(&mut self) {
        if let Err(e) = self.hotkeys.register_from_settings(&self.settings.settings().keyboard) {
            tracing::warn!("hotkey restore after settings close failed: {e}");
        }
    }

    /// Route a [`SettingsOutcome`] back into the App. Returns `true`
    /// when the outcome consumed the originating event, so the parent
    /// handler can short-circuit.
    fn dispatch_settings_outcome(&mut self, outcome: SettingsOutcome) -> bool {
        match outcome {
            SettingsOutcome::Ignored => false,
            SettingsOutcome::Handled => true,
            SettingsOutcome::Close => {
                self.settings_state.close();
                // Roll back any live-preview binding changes that
                // accumulated via RegisterHotkeys without being saved.
                self.restore_hotkeys_from_persisted();
                true
            }
            SettingsOutcome::OpenIdentityWizard => {
                self.settings_state.close();
                // Same rollback as Close — user exited settings via the
                // wizard launcher without pressing Save.
                self.restore_hotkeys_from_persisted();
                self.wizard = WizardState::SelectMethod;
                true
            }
            SettingsOutcome::OpenIdentityWizardAgent => {
                // Skip SelectMethod and kick straight into the
                // ssh-agent flow. The wizard's existing
                // SelectAgentKey screen will accept the picked key
                // via `select_agent_key`, which non-destructively
                // rewrites identity.json to point at it.
                self.settings_state.close();
                // Same rollback as Close.
                self.restore_hotkeys_from_persisted();
                self.wizard = WizardState::ConnectingAgent;
                self.spawn_connect_op();
                true
            }
            SettingsOutcome::OpenElevate => {
                self.settings_state.close();
                // Same rollback as Close.
                self.restore_hotkeys_from_persisted();
                self.elevate = Some(ElevateState::default());
                true
            }
            SettingsOutcome::CopyPublicKey(b64) => {
                self.copy_to_clipboard(b64, "Public key copied to clipboard");
                true
            }
            SettingsOutcome::PreviewSfx { kind, volume } => {
                self.backend.send(Command::PlaySfx { kind, volume });
                true
            }
            SettingsOutcome::RefreshDevices => {
                self.backend.send(Command::RefreshAudioDevices);
                true
            }
            SettingsOutcome::ResetStats => {
                self.backend.send(Command::ResetAudioStats);
                true
            }
            SettingsOutcome::Save(pending) => {
                self.apply_settings(pending);
                self.settings_state.close();
                true
            }
            SettingsOutcome::Apply(pending) => {
                self.apply_settings(pending);
                true
            }
            SettingsOutcome::PickDownloadDir => {
                self.spawn_pick_download_dir();
                true
            }
            SettingsOutcome::OpenDownloadDir(path) => {
                if let Err(e) = std::fs::create_dir_all(&path) {
                    tracing::warn!("download dir {:?} create failed: {e}", path);
                }
                open_path(&path);
                true
            }
            SettingsOutcome::Dispatch(commands) => {
                for cmd in commands {
                    self.backend.send(cmd);
                }
                true
            }
            SettingsOutcome::RegisterHotkeys(keyboard) => {
                // Live update: re-register against pending keyboard so
                // the new bindings take effect immediately (the user
                // can press them without closing the dialog). The
                // persisted store stays untouched until Save.
                if let Err(e) = self.hotkeys.register_from_settings(&keyboard) {
                    tracing::warn!("hotkey re-register failed: {e}");
                }
                true
            }
            SettingsOutcome::OpenPortalShortcutSettings => {
                self.hotkeys.open_portal_settings();
                true
            }
        }
    }

    /// Persist a [`PendingSettings`] snapshot: write the shared shell
    /// fields through `SettingsStore.modify`, dispatch backend commands
    /// for the runtime-mutating fields (audio settings, voice mode,
    /// device selection).
    fn apply_settings(&mut self, pending: settings::PendingSettings) {
        // Backend: audio + voice mode + device selection. These all
        // hit the audio task immediately rather than going through the
        // settings store, so we send them even when the value didn't
        // change — they're idempotent.
        self.backend.send(Command::UpdateAudioSettings {
            settings: pending.audio.clone(),
        });
        self.backend.send(Command::SetVoiceMode {
            mode: VoiceMode::from(pending.voice_mode),
        });
        self.backend.send(Command::SetInputDevice {
            device_id: pending.input_device.clone(),
        });
        self.backend.send(Command::SetOutputDevice {
            device_id: pending.output_device.clone(),
        });
        self.backend.send(Command::UpdateTxPipeline {
            config: pending.tx_pipeline.clone(),
        });
        // File-transfer bandwidth caps: UI edits in KiB/s, backend takes
        // bytes/sec (0 = unlimited). Applies live to in-flight transfers.
        self.backend.send(Command::SetFileTransferSpeedLimits {
            download_bps: (pending.download_speed_kbps as u64) * 1024,
            upload_bps: (pending.upload_speed_kbps as u64) * 1024,
        });

        // Shared shell store.
        self.settings.modify(|s| {
            // Audio + voice mode mirror what the backend will report
            // back; persisting them here means a restart re-applies
            // the same configuration.
            s.audio = (&pending.audio).into();
            // The TX pipeline lives nested inside `PersistentAudioSettings`
            // and the `From<&AudioSettings>` impl above stamps it back
            // to `None`, so we re-apply it after the conversion.
            s.audio.tx_pipeline = Some(pending.tx_pipeline.clone());
            s.voice_mode = pending.voice_mode;
            s.input_device_id = pending.input_device.clone();
            s.output_device_id = pending.output_device.clone();

            // Sounds.
            s.sfx.enabled = pending.sfx_enabled;
            s.sfx.volume = pending.sfx_volume.clamp(0.0, 1.0);
            s.sfx.disabled_sounds.clear();
            for (idx, kind) in rumble_client::SfxKind::all().iter().enumerate() {
                if !pending.sfx_kind_enabled.get(idx).copied().unwrap_or(true) {
                    s.sfx.disabled_sounds.insert(*kind);
                }
            }

            // Chat.
            s.chat.show_timestamps = pending.show_timestamps;
            s.chat.timestamp_format = pending.timestamp_format;
            s.chat.auto_sync_history = pending.auto_sync_history;
            s.chat.gif_autoplay = pending.gif_autoplay;

            // Files. Rule rows whose pattern is blank get dropped on
            // save — they could only have been left empty as scratch
            // space while the user was editing, and a rule with no
            // pattern can never match an offer anyway.
            s.file_transfer.auto_download_enabled = pending.auto_download_enabled;
            s.file_transfer.auto_download_rules = pending
                .auto_download_rules
                .iter()
                .map(|r| r.to_rule())
                .filter(|r| !r.mime_pattern.is_empty())
                .collect();
            s.file_transfer.download_speed_limit = (pending.download_speed_kbps as u64) * 1024;
            s.file_transfer.upload_speed_limit = (pending.upload_speed_kbps as u64) * 1024;
            s.file_transfer.download_dir = pending.download_dir.clone();

            // Shortcuts. Already re-registered on each edit via
            // RegisterHotkeys; here we just flush the pending snapshot
            // to disk so the bindings survive a restart.
            s.keyboard = pending.keyboard.clone();

            // Autoconnect: only meaningful once we have a recent server
            // to point at, so reuse the most-recent entry's address. If
            // there isn't one yet we just store the flag intent by
            // marking the username on whatever current addr we have —
            // the actual auto-connect resolver runs at startup.
            if pending.autoconnect {
                let target = s
                    .recent_servers
                    .iter()
                    .max_by_key(|r| r.last_used_unix)
                    .map(|r| r.addr.clone());
                if let Some(addr) = target {
                    s.auto_connect_addr = Some(addr);
                } else {
                    // No recent server yet — clear so we don't claim
                    // to autoconnect to nothing.
                    s.auto_connect_addr = None;
                }
            } else {
                s.auto_connect_addr = None;
            }
        });

        self.backend.send(Command::LocalMessage {
            text: "Settings saved.".to_string(),
        });
    }

    /// Test/scene-dump escape hatch: pretend the identity wizard is
    /// satisfied so callers can render scenes that aren't supposed to
    /// be obscured by it (every scene in `dump_bundles`, every test).
    pub fn suppress_first_run_for_test(&mut self) {
        self.wizard = WizardState::NotNeeded;
        self.unlock = UnlockState::default();
    }

    /// Test/scene-dump hook: drive the wizard into a specific state so
    /// `dump_bundles` can render every wizard screen for visual review.
    pub fn set_wizard_state_for_test(&mut self, state: WizardState) {
        self.wizard = state;
    }

    /// Test/scene-dump hook: install a plaintext local identity so
    /// `dump_bundles` can render the Connection-tab Identity section
    /// for a non-blank user. Uses a **fixed** signing key (not
    /// `generate_local_key`, which is random) so the rendered fingerprint
    /// and public key are reproducible across runs — golden snapshots
    /// depend on it.
    pub fn set_local_identity_for_test(&mut self) {
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[0x42; 32]);
        let _ = self.identity.manager_mut().import_signing_key(signing_key);
        self.identity.refresh_public_key();
    }

    /// Test/scene-dump hook: install an ssh-agent–bound identity. No
    /// real agent is contacted; we just write a `KeySource::SshAgent`
    /// config so the Connection-tab Identity panel renders its
    /// ssh-agent branch with a deterministic fingerprint and comment.
    pub fn set_ssh_agent_identity_for_test(&mut self) {
        use std::fmt::Write;

        use rumble_desktop_shell::{KeyConfig, KeySource, compute_fingerprint};
        let pubkey: [u8; 32] = [
            0x9c, 0xa3, 0x1f, 0x42, 0xb7, 0x6d, 0x5e, 0x11, 0x88, 0x44, 0xfa, 0x21, 0x07, 0xc0, 0x8e, 0x35, 0x6b, 0x52,
            0x2d, 0xae, 0x91, 0x70, 0xf8, 0x4c, 0xd3, 0x29, 0x66, 0xb5, 0x47, 0x1a, 0x0e, 0xff,
        ];
        let mut public_key_hex = String::with_capacity(64);
        for b in pubkey {
            let _ = write!(public_key_hex, "{b:02x}");
        }
        let config = KeyConfig {
            source: KeySource::SshAgent {
                fingerprint: compute_fingerprint(&pubkey),
                comment: "alice@workstation".to_string(),
            },
            public_key_hex,
        };
        self.identity.manager_mut().set_config(config, None);
        self.identity.refresh_public_key();
    }

    /// Test/scene-dump hook for the encrypted-key unlock prompt. The
    /// prompt is normally gated on `Identity::needs_unlock()`; this also
    /// flips the test override so a fresh on-disk identity still
    /// produces the modal.
    pub fn set_unlock_state_for_test(&mut self, state: UnlockState) {
        self.unlock = state;
        self.force_unlock_for_test = true;
    }

    /// Test/scene-dump hook for the sudo elevation prompt.
    pub fn set_elevate_state_for_test(&mut self, state: ElevateState) {
        self.elevate = Some(state);
    }

    /// Test/scene-dump hook for the toolbar transmission-mode dropdown.
    pub fn set_voice_mode_menu_open_for_test(&mut self, open: bool) {
        self.voice_mode_menu_open = open;
    }

    /// Test/scene-dump hook to seed the chat composer text, e.g. `"/"` to
    /// render the slash-command suggestion list.
    pub fn set_chat_input_for_test(&mut self, text: impl Into<String>) {
        self.composer.input = text.into();
    }

    /// Highlight a slash-command suggestion row, as Up/Down arrows would.
    /// Lets `dump_bundles` snapshot the keyboard-navigated list state.
    pub fn set_chat_command_selected_for_test(&mut self, idx: Option<usize>) {
        self.composer.command_selected = idx;
    }

    /// Test/scene-dump hook for the settings dialog. Snapshots the
    /// current backend audio state + shared shell settings into the
    /// settings UI state and forces the requested tab to active.
    pub fn open_settings_for_test(&mut self, tab: settings::SettingsTab) {
        let snapshot = self.backend.state();
        self.settings_state.open_with(&snapshot.audio, self.settings.settings());
        self.settings_state.tab = Some(tab);
    }

    /// Test/scene-dump hook to seed the saved-server list. Replaces the
    /// shared shell's `recent_servers` so the disconnected center area
    /// renders against a deterministic fixture.
    pub fn set_recent_servers_for_test(&mut self, servers: Vec<RecentServer>) {
        self.settings.modify(|s| {
            s.recent_servers = servers;
        });
    }

    /// Test/scene-dump hook for the saved-server add/edit modal.
    pub fn set_server_form_for_test(&mut self, form: ServerForm) {
        self.server_picker.form = Some(form);
    }

    /// Test/scene-dump hook for the timestamp-format dropdown inside
    /// the settings dialog. Used to render the Chat tab with its
    /// dropdown menu open.
    pub fn open_settings_dropdown_for_test(&mut self, which: settings::OpenSelect) {
        self.settings_state.open_select = which;
    }

    /// Test/scene-dump hook for the Admin tab — sets which group's
    /// accordion is expanded so a scene can snapshot the chip list +
    /// base-perm grid without driving a click event first.
    pub fn set_admin_expanded_group_for_test(&mut self, group: Option<String>) {
        self.settings_state.admin.expanded_group = group;
    }

    /// Test/scene-dump hook for the per-room Permissions editor modal.
    /// Builds the modal state directly from the current backend
    /// snapshot (no context-menu click needed). Expands the first
    /// local rule so the dump includes the tri-state grid.
    pub fn open_room_acl_modal_for_test(&mut self, room_id: uuid::Uuid) {
        let snapshot = self.backend.state();
        if let Some(mut modal) = room_acl::RoomAclModalState::open_for(&snapshot, room_id) {
            if !modal.entries.is_empty() {
                modal.expanded_entry = Some(0);
            }
            self.room_acl_modal = Some(modal);
        }
    }

    /// Test/scene-dump hook to seed the chat image-preview cache. The
    /// runtime path runs through `media_cache.on_event`, but bundle
    /// dumps don't have a real file-transfer plugin behind them — so
    /// let the scene-builder push a synthetic decoded image directly.
    pub fn insert_image_preview_for_test(&mut self, transfer_id: impl Into<String>, img: Image) {
        self.media_cache
            .image_cache_mut()
            .insert(transfer_id.into(), chat::CachedImage::Static(img));
    }

    /// Test/scene-dump hook to open the lightbox without a click event.
    /// Mirrors `open_lightbox` but skips the chat-history lookup so the
    /// scene can target a synthetic offer directly.
    pub fn open_lightbox_for_test(&mut self, transfer_id: impl Into<String>, name: impl Into<String>) {
        self.lightboxes.image = Some(chat::Lightbox::new(transfer_id, name));
    }

    /// Test/scene-dump hook to set the lightbox's zoom + pan directly,
    /// for snapshotting the controls in non-default state. Caller is
    /// responsible for setting them to plausible values; the runtime
    /// path goes through `Lightbox::set_zoom` / drag handlers and so
    /// always stays in range.
    pub fn set_lightbox_zoom_for_test(&mut self, zoom: f32, pan: (f32, f32)) {
        if let Some(lb) = self.lightboxes.image.as_mut() {
            lb.fit_to_window = false;
            lb.zoom = zoom;
            lb.pan = pan;
        }
    }

    /// Test/scene-dump hook to seed a model poster thumbnail. The runtime
    /// path renders it offscreen in `before_paint`; bundle dumps have no
    /// GPU device, so push a synthetic poster directly.
    pub fn insert_model_thumb_for_test(&mut self, transfer_id: impl Into<String>, img: Image) {
        self.media_cache.model_thumbs_mut().insert(transfer_id.into(), img);
    }

    /// Test/scene-dump hook to seed parsed model geometry, so the lightbox
    /// scene has a `chart3d` to project without a real download + parse.
    pub fn insert_model_for_test(&mut self, transfer_id: impl Into<String>, model: model::LoadedModel) {
        self.media_cache.models_mut().insert(transfer_id.into(), model);
    }

    /// Test/scene-dump hook to open the model lightbox without a click.
    /// Mirrors `open_model_lightbox` but takes the geometry directly.
    pub fn open_model_lightbox_for_test(
        &mut self,
        transfer_id: impl Into<String>,
        name: impl Into<String>,
        model: model::LoadedModel,
    ) {
        self.lightboxes.model = Some(model::ActiveModel::new(transfer_id, name, model));
    }
}

// ---------- view helpers ----------

const CHAT_SIDEBAR_HANDLE: &str = "chat-sidebar:resize";

// Toolbar icon glyphs. Reused from the bundled Mumble theme — the
// Mumble paint baked into each SVG is the visual signal we want
// (red for mute, green for active sound, orange for PTT), so we
// `parse` (not `parse_current_color`) and skip `.text_color(...)`.
static SVG_TB_MIC_ON: LazyLock<SvgIcon> =
    LazyLock::new(|| SvgIcon::parse(include_str!("../assets/icons/talking_off.svg")).expect("talking_off.svg parses"));
static SVG_TB_MIC_OFF: LazyLock<SvgIcon> =
    LazyLock::new(|| SvgIcon::parse(include_str!("../assets/icons/muted_self.svg")).expect("muted_self.svg parses"));
static SVG_TB_SOUND_ON: LazyLock<SvgIcon> = LazyLock::new(|| {
    SvgIcon::parse(include_str!("../assets/icons/self_undeafened.svg")).expect("self_undeafened.svg parses")
});
static SVG_TB_SOUND_OFF: LazyLock<SvgIcon> = LazyLock::new(|| {
    SvgIcon::parse(include_str!("../assets/icons/deafened_self.svg")).expect("deafened_self.svg parses")
});
static SVG_TB_MODE_CONT: LazyLock<SvgIcon> =
    LazyLock::new(|| SvgIcon::parse(include_str!("../assets/icons/talking_off.svg")).expect("talking_off.svg parses"));
static SVG_TB_MODE_PTT: LazyLock<SvgIcon> = LazyLock::new(|| {
    SvgIcon::parse(include_str!("../assets/icons/muted_pushtomute.svg")).expect("muted_pushtomute.svg parses")
});
static SVG_TB_DISCONNECT: LazyLock<SvgIcon> =
    LazyLock::new(|| SvgIcon::parse(include_str!("../assets/icons/disconnect.svg")).expect("disconnect.svg parses"));

const KEY_TB_VOICE_MODE: &str = "toolbar:voice-mode";

fn top_toolbar(state: &State, in_flight_uploads: usize, in_flight_downloads: usize) -> El {
    let connected = matches!(state.connection, ConnectionState::Connected { .. });

    let status = match &state.connection {
        ConnectionState::Disconnected => badge("Disconnected").muted(),
        ConnectionState::Connecting { server_addr } => badge(format!("Connecting to {server_addr}…")).warning(),
        ConnectionState::Connected { server_name, .. } => badge(server_name.clone()).success(),
        ConnectionState::ConnectionLost { error } => badge(format!("Connection lost: {error}")).destructive(),
        ConnectionState::CertificatePending { cert_info } => {
            badge(format!("Cert pending: {}", cert_info.server_name)).warning()
        }
    };

    let mut children: Vec<El> = vec![text("Rumble").title(), status, spacer()];

    let total_in_flight = in_flight_uploads + in_flight_downloads;
    if total_in_flight > 0 {
        let tooltip_text = match (in_flight_uploads, in_flight_downloads) {
            (u, 0) => format!("{u} upload(s)"),
            (0, d) => format!("{d} download(s)"),
            (u, d) => format!("{u} upload(s), {d} download(s)"),
        };
        let transfer_badge = row([
            icon(IconName::Upload).icon_size(tokens::ICON_SM),
            text(total_in_flight.to_string()).font_size(tokens::TEXT_SM.size),
        ])
        .key("toolbar:transfers")
        .gap(tokens::SPACE_1)
        .align(Align::Center)
        .padding(Sides::xy(tokens::SPACE_2, 0.0))
        .tooltip(tooltip_text);
        children.push(transfer_badge);
    }

    if connected {
        let (mute_icon, mute_tip) = if state.audio.self_muted {
            (SVG_TB_MIC_OFF.clone(), "Unmute microphone")
        } else {
            (SVG_TB_MIC_ON.clone(), "Mute microphone")
        };
        children.push(icon_button(mute_icon).key("toolbar:mute").tooltip(mute_tip));

        let (deafen_icon, deafen_tip) = if state.audio.self_deafened {
            (SVG_TB_SOUND_OFF.clone(), "Undeafen")
        } else {
            (SVG_TB_SOUND_ON.clone(), "Deafen")
        };
        children.push(icon_button(deafen_icon).key("toolbar:deafen").tooltip(deafen_tip));

        let (mode_icon, mode_tip) = match state.audio.voice_mode {
            VoiceMode::PushToTalk => (SVG_TB_MODE_PTT.clone(), "Voice mode: Push-to-Talk"),
            VoiceMode::Continuous => (SVG_TB_MODE_CONT.clone(), "Voice mode: Continuous"),
        };
        children.push(voice_mode_trigger(mode_icon, mode_tip));

        children.push(
            icon_button(SVG_TB_DISCONNECT.clone())
                .key("toolbar:disconnect")
                .tooltip("Disconnect from server"),
        );
    }
    // When disconnected, the center area renders the saved-server picker
    // (with its own "Add server…" / Connect / Edit / Remove controls), so
    // we don't add a redundant toolbar-level connect entry point here.

    children.push(
        icon_button(IconName::Settings)
            .key("toolbar:settings")
            .tooltip("Settings"),
    );

    row(children)
        .gap(tokens::SPACE_2)
        .padding(Sides::xy(tokens::SPACE_4, tokens::SPACE_2))
        .height(Size::Fixed(56.0))
        .width(Size::Fill(1.0))
        .fill(tokens::ACCENT)
        .align(Align::Center)
}

/// Thin fault banner shown directly under the toolbar whenever an audio
/// device fault is active. The session can look "connected" while the
/// microphone or speaker is actually dead, so we surface that prominently
/// here rather than burying it in Settings.
///
/// Each side renders a leading [`IconName::AlertCircle`] plus a status
/// badge that reuses the same `.destructive()` / `.warning()` treatment
/// as the toolbar connection badge:
/// - `recovering = false` (failed selection / abandoned recovery) →
///   `.destructive()` ("Microphone error: …" / "Speaker error: …").
/// - `recovering = true` (task is retrying the device) → `.warning()`
///   ("Microphone disconnected — reconnecting…").
///
/// Returns `None` when both `input_fault` and `output_fault` are `None`,
/// so nothing is rendered (and the column collapses) on a healthy session.
fn fault_banner(state: &State) -> Option<El> {
    let mut badges: Vec<El> = Vec::new();

    if let Some(fault) = &state.audio.input_fault {
        let b = if fault.recovering {
            badge("Microphone disconnected — reconnecting…").warning()
        } else {
            badge(format!("Microphone error: {}", fault.message)).destructive()
        };
        badges.push(
            row([icon(IconName::AlertCircle).icon_size(tokens::ICON_SM), b])
                .key("fault:input")
                .gap(tokens::SPACE_1)
                .align(Align::Center),
        );
    }

    if let Some(fault) = &state.audio.output_fault {
        let b = if fault.recovering {
            badge("Speaker disconnected — reconnecting…").warning()
        } else {
            badge(format!("Speaker error: {}", fault.message)).destructive()
        };
        badges.push(
            row([icon(IconName::AlertCircle).icon_size(tokens::ICON_SM), b])
                .key("fault:output")
                .gap(tokens::SPACE_1)
                .align(Align::Center),
        );
    }

    if badges.is_empty() {
        return None;
    }

    Some(
        row(badges)
            .key("fault:banner")
            .gap(tokens::SPACE_3)
            .padding(Sides::xy(tokens::SPACE_4, tokens::SPACE_2))
            .width(Size::Fill(1.0))
            .fill(tokens::SECONDARY)
            .stroke(tokens::BORDER)
            .align(Align::Center),
    )
}

/// Voice-mode dropdown trigger: an icon button paired with a chevron
/// so it reads as a select. The trigger key is `KEY_TB_VOICE_MODE`;
/// the popover (`voice_mode_menu`) anchors below it.
fn voice_mode_trigger(mode_icon: SvgIcon, tooltip_text: &str) -> El {
    row([
        icon(mode_icon).icon_size(tokens::ICON_SM),
        icon(IconName::ChevronDown)
            .icon_size(tokens::ICON_SM)
            .text_color(tokens::MUTED_FOREGROUND),
    ])
    .key(KEY_TB_VOICE_MODE)
    .gap(tokens::SPACE_1)
    .align(Align::Center)
    .padding(Sides::xy(tokens::SPACE_2, 0.0))
    .height(Size::Fixed(tokens::CONTROL_HEIGHT))
    .fill(tokens::SECONDARY)
    .stroke(tokens::BORDER)
    .radius(tokens::RADIUS_MD)
    .focusable()
    .paint_overflow(Sides::all(tokens::RING_WIDTH))
    .cursor(Cursor::Pointer)
    .tooltip(tooltip_text)
}

/// Voice-mode dropdown popover panel. Composed at root via `popover`
/// so it paints over the toolbar surface.
fn voice_mode_menu(current: VoiceMode) -> El {
    use damascene_core::widgets::popover::{Anchor, menu_item, popover, popover_panel};
    let cont_label = if current == VoiceMode::Continuous {
        "Continuous (VAD) ✓"
    } else {
        "Continuous (VAD)"
    };
    let ptt_label = if current == VoiceMode::PushToTalk {
        "Push-to-Talk ✓"
    } else {
        "Push-to-Talk"
    };
    popover(
        KEY_TB_VOICE_MODE,
        Anchor::below_key(KEY_TB_VOICE_MODE),
        popover_panel([
            menu_item(cont_label).key(format!("{KEY_TB_VOICE_MODE}:option:cont")),
            menu_item(ptt_label).key(format!("{KEY_TB_VOICE_MODE}:option:ptt")),
        ]),
    )
}

fn center_area(state: &State, recent_servers: &[RecentServer], room_tree: &RoomTreeState) -> El {
    if matches!(state.connection, ConnectionState::Connected { .. }) {
        room_tree::render(room_tree, state)
    } else {
        server_picker::render_center(state, recent_servers)
    }
}

fn cert_modal(cert_info: &PendingCertificate) -> El {
    modal(
        "cert",
        "Untrusted certificate",
        [
            alert([
                alert_title("Self-signed or unknown certificate"),
                alert_description(
                    "Only accept if this fingerprint matches what the server administrator gave you. Once accepted, \
                     the certificate is saved for future connections.",
                ),
            ])
            .warning(),
            field_row("Server", text(cert_info.server_addr.clone()).semibold()),
            field_row("Certificate for", text(cert_info.server_name.clone()).semibold()),
            text("Fingerprint (SHA-256)").muted(),
            // SHA-256 hex with colon separators is 79 chars wide —
            // wrap_text() so it flows across two lines instead of
            // overflowing the modal. The user needs to read the full
            // hash, so .ellipsis() would be wrong here.
            mono(cert_info.fingerprint_hex())
                .font_size(tokens::TEXT_XS.size)
                .wrap_text(),
            row([
                button("Reject").key("cert:reject"),
                spacer(),
                button("Trust and connect").key("cert:accept").primary(),
            ])
            .gap(tokens::SPACE_2)
            .width(Size::Fill(1.0))
            .align(Align::Center),
        ],
    )
}

/// Translucent full-window scrim with a centred "Drop to share"
/// banner. Rendered while the OS is reporting a hovered file so the
/// user gets a visible cue that the window will accept the drop.
fn drop_target_hint() -> El {
    let banner = column([
        icon(IconName::Upload).font_size(28.0),
        text("Drop to share").title(),
        text("The file will be uploaded to the current room.")
            .muted()
            .font_size(tokens::TEXT_XS.size),
    ])
    .gap(tokens::SPACE_2)
    .align(Align::Center)
    .padding(Sides::all(tokens::SPACE_5))
    .fill(tokens::POPOVER)
    .stroke(tokens::PRIMARY)
    .stroke_width(2.0)
    .radius(tokens::RADIUS_LG);

    // The centred-banner container carries the translucent scrim fill
    // itself — no separate full-size scrim layer needed.
    stack([banner])
        .fill(tokens::OVERLAY_SCRIM)
        .fill_size()
        .padding(Sides::all(tokens::SPACE_7))
        .align(Align::Center)
}

/// Open `path` with the OS default application. Fires and forgets;
/// errors are logged but not surfaced in the UI.
fn open_path(path: &Path) {
    #[cfg(target_os = "linux")]
    let (bin, args): (&str, &[&std::ffi::OsStr]) = ("xdg-open", &[path.as_os_str()]);
    #[cfg(target_os = "macos")]
    let (bin, args): (&str, &[&std::ffi::OsStr]) = ("open", &[path.as_os_str()]);
    #[cfg(target_os = "windows")]
    let (bin, args): (&str, &[&std::ffi::OsStr]) = ("explorer", &[path.as_os_str()]);

    if let Err(e) = std::process::Command::new(bin).args(args).spawn() {
        tracing::error!("open_path {:?}: {e}", path);
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use rumble_client::{ChatMessage, ChatMessageKind, ChatMessageVisibility};

    use super::*;

    fn make_msg(id: [u8; 16]) -> ChatMessage {
        ChatMessage {
            id,
            sender: String::new(),
            sender_id: None,
            text: String::new(),
            timestamp: SystemTime::UNIX_EPOCH,
            kind: ChatMessageKind::default(),
            attachment: None,
            visibility: ChatMessageVisibility::Normal,
        }
    }

    // First non-empty observation seeds silently: the connect-time backlog
    // must not produce new messages.
    #[test]
    fn first_observation_populated_seeds_without_new() {
        let msgs = [make_msg([1u8; 16]), make_msg([2u8; 16])];
        let (new, seen) = diff_new_chat_messages(&msgs, &None);
        assert!(new.is_empty(), "seeding must not produce new messages");
        let seen = seen.expect("non-empty observation must seed");
        assert_eq!(seen.len(), 2);
        assert!(seen.contains(&[1u8; 16]));
        assert!(seen.contains(&[2u8; 16]));
    }

    // Empty observations stay unseeded, so the eventual backlog (which may
    // arrive many frames after app start) is still absorbed silently; only
    // arrivals after that first batch are detected as new.
    #[test]
    fn empty_observation_stays_unseeded_until_backlog() {
        let (new_seed, seen_seed) = diff_new_chat_messages(&[], &None);
        assert!(new_seed.is_empty());
        assert!(seen_seed.is_none(), "empty observation must not seed");

        // Many frames later: the connect-time backlog lands — absorbed.
        let backlog = [make_msg([1u8; 16]), make_msg([2u8; 16])];
        let (new, seen) = diff_new_chat_messages(&backlog, &seen_seed);
        assert!(new.is_empty(), "backlog must be absorbed, not treated as new");
        let seen = seen.expect("backlog must seed");

        // A genuinely-live arrival after seeding is detected.
        let msgs = [make_msg([1u8; 16]), make_msg([2u8; 16]), make_msg([3u8; 16])];
        let (new, _) = diff_new_chat_messages(&msgs, &Some(seen));
        assert_eq!(new.len(), 1);
        assert_eq!(new[0].id, [3u8; 16]);
    }

    // Window-slide: when the 100-entry cap drains old ids from the front, newly
    // appended ids at the tail are detected while evicted ids are not re-fired.
    #[test]
    fn window_slide_drops_old_ids_detects_new_tail() {
        // Ids 0–99 are in the seen set (previous snapshot).
        let prev_seen: HashSet<[u8; 16]> = (0u8..100)
            .map(|i| {
                let mut id = [0u8; 16];
                id[0] = i;
                id
            })
            .collect();

        // Snapshot now holds ids 1–100 (id 0 slid out, id 100 is new).
        let msgs: Vec<ChatMessage> = (1u8..=100)
            .map(|i| {
                let mut id = [0u8; 16];
                id[0] = i;
                make_msg(id)
            })
            .collect();

        let (new, updated) = diff_new_chat_messages(&msgs, &Some(prev_seen));
        assert_eq!(new.len(), 1, "only id 100 must be new");
        assert_eq!(new[0].id[0], 100);

        // Updated set reflects the snapshot, not the old seen set.
        let updated = updated.expect("diff against a seeded set stays seeded");
        assert_eq!(updated.len(), 100);
        let evicted = {
            let mut id = [0u8; 16];
            id[0] = 0;
            id
        };
        assert!(!updated.contains(&evicted), "evicted id must not be in updated set");
    }

    // Re-sorted order: a HistoryMerged can insert a message anywhere in the list.
    // The new id must be detected regardless of its position.
    #[test]
    fn resorted_middle_insertion_is_detected() {
        let prev_seen: HashSet<[u8; 16]> = [[1u8; 16], [3u8; 16]].into_iter().collect();

        let middle_id = {
            let mut id = [0u8; 16];
            id[0] = 2;
            id
        };
        // Timestamp-sorted order: id 1, id 2, id 3.
        let msgs = [make_msg([1u8; 16]), make_msg(middle_id), make_msg([3u8; 16])];

        let (new, _) = diff_new_chat_messages(&msgs, &Some(prev_seen));
        assert_eq!(new.len(), 1);
        assert_eq!(new[0].id, middle_id);
    }
}
