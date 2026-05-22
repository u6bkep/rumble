//! Top-level aetna `App` for the Rumble client.
//!
//! Owns local UI state (connect form fields, modal flags, selected
//! room) and projects `(state, ui_state) -> El` on every frame.

use std::{
    collections::{HashMap, HashSet},
    io::BufReader,
    path::{Path, PathBuf},
    sync::LazyLock,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use aetna_core::prelude::*;
use aetna_winit_wgpu::WinitWgpuApp;

use rumble_client::{
    ProcessorRegistry, SfxKind, build_default_tx_pipeline, merge_with_default_tx_pipeline, register_builtin_processors,
};
use rumble_desktop_shell::{
    AcceptedCertificate, RecentServer, SettingsStore,
    hotkeys::{HotkeyEvent, HotkeyManager},
    identity::{connect_and_list_keys, generate_and_add_to_agent},
};
use rumble_protocol::{AudioSettings, Command, ConnectionState, PendingCertificate, State, VoiceMode};
use tokio::{runtime::Runtime, task::JoinHandle};

use crate::{
    animated_gpu::AnimatedGpu,
    backend::UiBackend,
    chat,
    elevate::{self, ElevateOutcome, ElevateState},
    identity::Identity,
    room_acl,
    room_tree::{self, RoomTreeOutcome, RoomTreeState},
    server_picker::{self, ServerForm, ServerPickerOutcome, ServerPickerState},
    settings::{self, SettingsOutcome, SettingsState},
    video,
    wizard::{self, PendingAgentOp, UnlockState, WizardOutcome, WizardState},
};

/// Result yielded by `pending_video_open`: `(transfer_id, file_name, stream)` or libmpv error.
type PendingVideoOpenResult = Result<(String, String, rumble_video::VideoStream), rumble_video::Error>;

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

    chat_input: String,

    /// Chat sidebar width in logical pixels — adjusted by dragging
    /// the divider on its right edge. Initialized from
    /// [`tokens::SIDEBAR_WIDTH`] (the conventional ~256px starting
    /// point) and clamped to [`tokens::SIDEBAR_WIDTH_MIN`] /
    /// `_MAX` by the resize handler.
    chat_sidebar_w: f32,
    chat_sidebar_drag: ResizeDrag,

    /// Audio-processor factory registry. Owned by the App so the
    /// settings dialog can read each processor's display name,
    /// description and JSON schema when rendering the Processing tab.
    /// Built once in [`Self::new`] from `register_builtin_processors`.
    processor_registry: ProcessorRegistry,

    /// Room tree view + its ephemeral state (selection, context menus,
    /// drag-and-drop, confirmation modals). See [`crate::room_tree`].
    room_tree: RoomTreeState,

    /// `state.chat_messages.len()` at the previous frame. Used to detect
    /// new arrivals so we can fire `SfxKind::Message` once per batch.
    prev_chat_count: usize,

    /// `connection.is_connected()` at the previous frame, to fire
    /// `SfxKind::Connect`/`Disconnect` on transitions.
    prev_connected: bool,

    /// Our room id and the remote user ids in it at the previous frame,
    /// to fire `SfxKind::UserJoin`/`UserLeave`. Reseeded without firing
    /// when our room id changes (room switch, connect, disconnect).
    prev_room_id: Option<uuid::Uuid>,
    prev_room_members: HashSet<u64>,

    /// Room id of a locally-initiated `JoinRoom` awaiting confirmation, so
    /// `pump_sfx` can tell our own channel switch (`SelfChannelJoin`) apart
    /// from being relocated by an admin/other user (`SelfChannelMoved`).
    pending_self_join: Option<uuid::Uuid>,

    /// Our `server_muted` flag at the previous frame, to fire
    /// `SfxKind::ServerMute` when an admin mutes us server-side.
    prev_server_muted: bool,

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

    /// Decoded image cache keyed by `transfer_id`. Populated in
    /// `before_build` once a file transfer reports `is_finished` with
    /// a `local_path`; the chat module reads from this map to swap an
    /// inline preview in for the file card. Static images are a single
    /// frame; GIFs decode their full frame sequence so playback
    /// doesn't re-decode.
    image_cache: chat::ImageCache,

    /// `transfer_id`s we have tried to decode and failed (corrupt,
    /// truncated, unsupported codec). Recorded so the next frame
    /// doesn't keep re-attempting the same decode.
    image_failed: HashSet<String>,

    /// Per-message GIF playback state keyed by `transfer_id`. One
    /// entry per animated cache entry; the inline preview and the
    /// lightbox both read from this so they stay in lockstep. Seeded
    /// when [`Self::pump_image_cache`] inserts an animated entry.
    gif_playback: HashMap<String, chat::GifPlayback>,

    /// Click-to-enlarge image viewer state. `Some` when a chat image
    /// preview was clicked; cleared by Close / Escape / scrim click.
    /// The image itself comes from [`Self::lightbox_full`] (a
    /// dedicated full-resolution decode) or, until that decode lands,
    /// the thumbnail in [`Self::image_cache`].
    image_lightbox: Option<chat::Lightbox>,

    /// Full-resolution image (or animated frame sequence) for the
    /// active lightbox. Decoded off-thread on lightbox open and
    /// dropped on close, so the 1024-capped thumbnail in
    /// `image_cache` no longer caps lightbox quality. `None` until
    /// the decode completes (the lightbox falls back to the
    /// thumbnail in the meantime), or when the source failed to
    /// decode.
    lightbox_full: Option<chat::CachedImage>,

    /// In-flight full-resolution decode spawned on `runtime`. Polled
    /// each frame in `poll_lightbox_decode`; the result lands in
    /// `lightbox_full`. Aborted (and never observed) when the
    /// lightbox closes before the decode finishes.
    pending_lightbox_decode: Option<JoinHandle<Result<chat::CachedImage, image::ImageError>>>,

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
    /// once at startup, used in [`Self::sync_animated_gpu`] to lazily
    /// allocate per-message textures for animated previews. `None`
    /// before the host has finished bringing up wgpu (e.g. while
    /// running tests against a `MockUiBackend`).
    gpu_device: Option<wgpu::Device>,

    /// Per-message GPU mirrors for animated previews keyed by
    /// `transfer_id`. Populated lazily by [`Self::sync_animated_gpu`]
    /// the first time `before_paint` runs with an active
    /// [`chat::GifPlayback`] for an animated cache entry. Dropped
    /// alongside its `gif_playback` slot so the GPU memory tracks the
    /// CPU-side cache.
    animated_gpu: HashMap<String, AnimatedGpu>,

    /// Currently-open video lightbox, if any. Owns the libmpv
    /// stream + decode worker + GPU mirror; dropped on close to
    /// free everything in one shot. Only one video plays at a
    /// time — opening a second supersedes the first.
    active_video: Option<video::ActiveVideo>,
    /// In-flight `VideoStream::open` task. `load_file` blocks for
    /// 10–50ms while libmpv negotiates the container, so opening
    /// is offloaded to the runtime to avoid stalling the UI; the
    /// completed stream lands in `active_video` next frame.
    pending_video_open: Option<JoinHandle<PendingVideoOpenResult>>,

    /// Decoded poster thumbnails for downloaded video files,
    /// keyed by `transfer_id`. Populated by [`Self::pump_video_thumbs`]
    /// after a one-shot libmpv decode of the first frame; chat
    /// consults this to swap a `file_offer_card` for a
    /// `video_preview` card when the entry is ready.
    video_thumbs: HashMap<String, Image>,
    /// Transfer ids whose thumbnail extraction has failed at
    /// least once (file format unsupported, decoder timeout,
    /// etc.). Mirrors [`Self::image_failed`] — keeps the pump
    /// from re-attempting on every frame.
    failed_video_thumbs: HashSet<String>,
    /// In-flight thumbnail-decode tasks keyed by `transfer_id`.
    /// Polled in [`Self::pump_video_thumbs`]; on completion the
    /// result lands in either `video_thumbs` (Ok) or
    /// `failed_video_thumbs` (Err).
    pending_video_thumbs: HashMap<String, JoinHandle<Result<Image, rumble_video::Error>>>,
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
            chat_input: String::new(),
            chat_sidebar_w: tokens::SIDEBAR_WIDTH,
            chat_sidebar_drag: ResizeDrag::default(),
            processor_registry,
            room_tree: RoomTreeState::default(),
            prev_chat_count: 0,
            prev_connected: false,
            prev_room_id: None,
            prev_room_members: HashSet::new(),
            pending_self_join: None,
            prev_server_muted: false,
            pending_file_dialog: None,
            auto_handled_offers: HashSet::new(),
            image_cache: HashMap::new(),
            image_failed: HashSet::new(),
            gif_playback: HashMap::new(),
            image_lightbox: None,
            lightbox_full: None,
            pending_lightbox_decode: None,
            file_context_menu: None,
            pending_save_as: None,
            pending_pick_download_dir: None,
            file_drop_hover: false,
            room_acl_modal: None,
            gpu_device: None,
            animated_gpu: HashMap::new(),
            active_video: None,
            pending_video_open: None,
            video_thumbs: HashMap::new(),
            failed_video_thumbs: HashSet::new(),
            pending_video_thumbs: HashMap::new(),
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

/// Cap on the longest edge of a cached chat thumbnail, in pixels.
/// ~1024 keeps the 220px-tall preview rect sharp on a 2× display
/// without burning RAM on phone-camera-sized sources.
const MAX_PREVIEW_PX: u32 = 1024;

/// Cap on the longest edge of a lightbox full-resolution decode, in
/// pixels. Generous enough that real-world photos go through
/// untouched but defensive against pathological inputs (and well
/// under the typical 8192 GPU texture limit).
const MAX_LIGHTBOX_PX: u32 = 4096;

/// Decode `path` into a [`chat::CachedImage`]. `max_px` caps the
/// longest edge of every emitted frame — `Some(n)` downsamples, `None`
/// loads the source at its natural resolution.
///
/// Animated GIF, animated WebP, and APNG decode every frame and produce
/// [`chat::CachedImage::Animated`]; single-frame animations and all
/// other formats collapse to `Static`.
fn decode_image(path: &Path, max_px: Option<u32>) -> Result<chat::CachedImage, image::ImageError> {
    use image::{
        AnimationDecoder, ImageFormat,
        codecs::{gif::GifDecoder, png::PngDecoder, webp::WebPDecoder},
    };

    let reader = image::ImageReader::open(path)?.with_guessed_format()?;
    match reader.format() {
        Some(ImageFormat::Gif) => {
            let decoder = GifDecoder::new(BufReader::new(std::fs::File::open(path)?))?;
            return collect_animated_frames(decoder.into_frames(), max_px);
        }
        Some(ImageFormat::WebP) => {
            let decoder = WebPDecoder::new(BufReader::new(std::fs::File::open(path)?))?;
            if decoder.has_animation() {
                return collect_animated_frames(decoder.into_frames(), max_px);
            }
        }
        Some(ImageFormat::Png) => {
            let decoder = PngDecoder::new(BufReader::new(std::fs::File::open(path)?))?;
            if decoder.is_apng()? {
                return collect_animated_frames(decoder.apng()?.into_frames(), max_px);
            }
        }
        _ => {}
    }

    let img = reader.decode()?;
    let img = match max_px {
        Some(cap) if img.width().max(img.height()) > cap => img.thumbnail(cap, cap),
        _ => img,
    };
    let rgba = img.to_rgba8();
    Ok(chat::CachedImage::Static(Image::from_rgba8(
        rgba.width(),
        rgba.height(),
        rgba.into_raw(),
    )))
}

/// Drain an animation decoder's [`image::Frames`] iterator, downsample
/// each by `max_px`, and pack the result into a
/// [`chat::CachedImage::Animated`]. A delay floor of 20ms per frame
/// keeps a 0/1-ms pathological file from pegging the playback pump
/// (browsers do the same — Chrome and Firefox both clamp tiny delays).
/// Single-frame inputs collapse to `Static`.
fn collect_animated_frames(
    frames: image::Frames<'_>,
    max_px: Option<u32>,
) -> Result<chat::CachedImage, image::ImageError> {
    use image::DynamicImage;

    let mut out: Vec<(Image, Duration)> = Vec::new();
    for frame in frames {
        let frame = frame?;
        let (numer, denom) = frame.delay().numer_denom_ms();
        let micros = (numer as u64).saturating_mul(1000) / (denom.max(1) as u64);
        let delay = Duration::from_micros(micros).max(Duration::from_millis(20));

        let mut dynimg = DynamicImage::ImageRgba8(frame.into_buffer());
        if let Some(cap) = max_px
            && dynimg.width().max(dynimg.height()) > cap
        {
            dynimg = dynimg.thumbnail(cap, cap);
        }
        let rgba = dynimg.to_rgba8();
        let img = Image::from_rgba8(rgba.width(), rgba.height(), rgba.into_raw());
        out.push((img, delay));
    }

    if out.len() <= 1 {
        // Single-frame source (rare but legal for any of these formats).
        // The animated UI overhead — playback state, controls overlay —
        // adds nothing here, so collapse to Static. An empty file would
        // have been refused by the decoder; the unwrap_or is a paranoid
        // fallback.
        return Ok(chat::CachedImage::Static(
            out.into_iter()
                .next()
                .map(|(img, _)| img)
                .unwrap_or_else(|| Image::from_rgba8(1, 1, vec![0, 0, 0, 0])),
        ));
    }

    Ok(chat::CachedImage::Animated { frames: out })
}

/// Backwards-compat wrapper for the chat thumbnail cache decode.
fn decode_thumbnail(path: &Path) -> Result<chat::CachedImage, image::ImageError> {
    decode_image(path, Some(MAX_PREVIEW_PX))
}

/// Advance an animated cache entry's playhead given elapsed wall-clock
/// time. Skips through any number of frames whose accumulated delay is
/// already in the past, so a long idle window resumes at the visually-
/// correct frame in one update rather than fast-forwarding through
/// every frame for one tick each.
fn advance_gif_frame(pb: &mut chat::GifPlayback, frames: &[(Image, Duration)], now: Instant) {
    let mut elapsed = now.saturating_duration_since(pb.last_advance);
    let mut idx = pb.frame_idx.min(frames.len() - 1);
    let mut advanced = false;
    loop {
        let cur_delay = frames[idx].1;
        if elapsed < cur_delay {
            break;
        }
        elapsed -= cur_delay;
        idx = (idx + 1) % frames.len();
        advanced = true;
    }
    if advanced {
        pb.frame_idx = idx;
        // Anchor `last_advance` at the frame boundary we landed on, so
        // residual sub-frame elapsed time doesn't accumulate drift.
        pb.last_advance = now - elapsed;
    }
}

impl<B: UiBackend> App for RumbleApp<B> {
    fn before_build(&mut self) {
        self.poll_agent_op();
        self.poll_file_dialog();
        self.poll_save_as();
        self.poll_pick_download_dir();
        self.pump_sfx();
        self.pump_image_cache();
        self.poll_lightbox_decode();
        self.pump_gif_animations();
        self.pump_video_thumbs();
        self.poll_video_open();
        self.pump_hotkeys();
        if let Some(active) = self.active_video.as_mut() {
            active.refresh_scrub_value();
        }
    }

    fn build(&self, _cx: &BuildCx) -> El {
        let state = self.backend.state();
        let shell = self.settings.settings();

        let transfers: chat::TransferMap = self
            .backend
            .transfers()
            .into_iter()
            .map(|t| (t.id.0.clone(), t))
            .collect();

        let main = column([
            top_toolbar(&state),
            row([
                chat::render(
                    &state,
                    &shell.chat,
                    &self.image_cache,
                    &self.gif_playback,
                    &self.animated_gpu,
                    &self.video_thumbs,
                    &transfers,
                    &self.chat_input,
                    &self.selection,
                    self.chat_sidebar_w,
                ),
                resize_handle(Axis::Row).key(CHAT_SIDEBAR_HANDLE),
                center_area(&state, &shell.recent_servers, &self.room_tree),
            ])
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0))
            .align(Align::Stretch),
        ])
        .fill_size()
        .align(Align::Stretch);

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
        // Suppress the server form whenever a higher-priority modal is up.
        let connect_layer = if !wizard_open && unlock_layer.is_none() && cert_layer.is_none() {
            server_picker::render_form_modal(&self.server_picker, &self.selection)
        } else {
            None
        };

        let voice_mode_layer = if self.voice_mode_menu_open
            && !wizard_open
            && unlock_layer.is_none()
            && cert_layer.is_none()
            && state.connection.is_connected()
        {
            Some(voice_mode_menu(state.audio.voice_mode))
        } else {
            None
        };

        let (settings_panel, settings_popover) = if !wizard_open && unlock_layer.is_none() && cert_layer.is_none() {
            settings::render(
                &self.settings_state,
                &state,
                &self.identity,
                &self.selection,
                &self.processor_registry,
                &self.hotkeys,
            )
        } else {
            (None, None)
        };

        let lower_layers_block_room_menus =
            wizard_open || unlock_layer.is_some() || cert_layer.is_some() || self.settings_state.open;
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
        let lightbox_layer = if !wizard_open
            && unlock_layer.is_none()
            && cert_layer.is_none()
            && !self.settings_state.open
            && let Some(lightbox_state) = self.image_lightbox.as_ref()
            && let Some(cached) = self
                .lightbox_full
                .as_ref()
                .or_else(|| self.image_cache.get(&lightbox_state.transfer_id))
        {
            let playback = self.gif_playback.get(&lightbox_state.transfer_id);
            let gpu = self.animated_gpu.get(&lightbox_state.transfer_id);
            Some(chat::render_lightbox(lightbox_state, cached, playback, gpu))
        } else {
            None
        };

        // Video lightbox sits in the same overlay slot as the
        // image lightbox — only one is ever open at a time, so
        // they don't visually conflict. Same modal-suppression
        // rules apply (no popping over wizard / cert / unlock /
        // settings).
        let video_lightbox_layer = if !wizard_open
            && unlock_layer.is_none()
            && cert_layer.is_none()
            && !self.settings_state.open
            && let Some(active) = self.active_video.as_ref()
        {
            Some(video::render_lightbox(active))
        } else {
            None
        };

        let file_ctx_layer =
            if !wizard_open && unlock_layer.is_none() && cert_layer.is_none() && !self.settings_state.open {
                self.file_context_menu.as_ref().map(chat::render_file_context_menu)
            } else {
                None
            };

        // Per-room ACL editor modal. Suppressed under cert / unlock /
        // wizard for the same reason settings is — those gate the
        // session itself. Renders above the settings dialog when both
        // are somehow open (shouldn't happen by normal flow).
        let (room_acl_modal_layer, room_acl_popover_layer) = if !wizard_open
            && unlock_layer.is_none()
            && cert_layer.is_none()
            && let Some(modal) = self.room_acl_modal.as_ref()
        {
            let layers = room_acl::render(modal, &state, &self.selection);
            (layers.modal, layers.popover)
        } else {
            (None, None)
        };

        // Drop-target hint while the OS is reporting a hovered file.
        // Suppressed under modals (the user can still drop a file then,
        // but the prompt would visually fight whatever modal is up).
        let drop_target_layer = if self.file_drop_hover
            && !wizard_open
            && unlock_layer.is_none()
            && cert_layer.is_none()
            && state.connection.is_connected()
        {
            Some(drop_target_hint())
        } else {
            None
        };

        // Sudo elevation prompt. Suppressed by the same session-gate
        // modals (wizard/unlock/cert). Renders above settings since the
        // App closes settings when opening it — no overlap by design,
        // but the explicit ordering keeps the precedence obvious.
        let elevate_layer = if !wizard_open
            && unlock_layer.is_none()
            && cert_layer.is_none()
            && let Some(es) = self.elevate.as_ref()
        {
            Some(elevate::render(es, &self.selection))
        } else {
            None
        };

        // Always wrap in `overlays(...)` — even with no app layers, the
        // root must be an `Axis::Overlay` container so aetna's runtime
        // tooltip layer overlays the main view instead of competing for
        // flex space (see vendor/aetna/.../tooltip.rs root precondition).
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

    fn on_event(&mut self, event: UiEvent) {
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
                self.backend.send(Command::LocalMessage {
                    text: format!("Sharing {}", path.display()),
                });
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
            resize_handle::apply_event_fixed(
                &mut self.chat_sidebar_w,
                &mut self.chat_sidebar_drag,
                &event,
                CHAT_SIDEBAR_HANDLE,
                Axis::Row,
                resize_handle::Side::Start,
                tokens::SIDEBAR_WIDTH_MIN,
                tokens::SIDEBAR_WIDTH_MAX,
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

        // Chat composer.
        if event.target_key() == Some(chat::KEY_INPUT) {
            // Send on Enter when not Shift-held. Slash commands route
            // through `parse_and_send_chat`; plain text falls through
            // to a `Command::SendChat`.
            if let UiEventKind::KeyDown = event.kind
                && let Some(kp) = event.key_press.as_ref()
                && matches!(kp.key, UiKey::Enter)
                && !kp.modifiers.shift
            {
                let trimmed = self.chat_input.trim().to_string();
                if !trimmed.is_empty() {
                    self.parse_and_send_chat(&trimmed);
                    self.chat_input.clear();
                    self.selection = Selection::default();
                }
                return;
            }
            text_input::apply_event(&mut self.chat_input, &mut self.selection, chat::KEY_INPUT, &event);
            return;
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
            self.backend.send(Command::CancelTransfer {
                transfer_id: transfer_id.to_string(),
            });
            return;
        }

        // Per-GIF play/pause and explicit-open-lightbox icons live on
        // top of the preview card (`stack([preview, controls])`), so
        // their routes win over the underlying `chat:preview:*` route.
        // Handle them before the body click below.
        if matches!(event.kind, UiEventKind::Click | UiEventKind::Activate)
            && let Some(route) = event.route()
        {
            if let Some(transfer_id) = chat::parse_gif_play_key(route) {
                self.toggle_gif_playback(transfer_id);
                return;
            }
            if let Some(transfer_id) = chat::parse_gif_lightbox_key(route) {
                self.open_lightbox(transfer_id);
                return;
            }
        }

        // Image lightbox. Open by clicking (or keyboard-activating) an
        // inline image preview; close via the panel's Close button, the
        // scrim, or Escape. Open/close are stateless beyond toggling
        // `image_lightbox` — the panel re-reads the image from
        // `image_cache` each frame so a transfer evicted out from under
        // an open lightbox dismisses it cleanly.
        if event.kind == UiEventKind::Click
            && let Some(route) = event.route()
            && let Some(transfer_id) = chat::parse_preview_key(route)
        {
            self.open_lightbox(transfer_id);
            return;
        }
        if self.image_lightbox.is_some()
            && (event.is_click_or_activate(chat::KEY_LIGHTBOX_CLOSE)
                || (event.is_route(chat::KEY_LIGHTBOX_DISMISS) && event.kind == UiEventKind::Click)
                || event.kind == UiEventKind::Escape)
        {
            self.close_lightbox();
            return;
        }
        let lightbox_image_size = self.image_lightbox.as_ref().and_then(|lightbox| {
            let cached = self
                .lightbox_full
                .as_ref()
                .or_else(|| self.image_cache.get(&lightbox.transfer_id))?;
            let playback = self.gif_playback.get(&lightbox.transfer_id);
            Some(cached.current_frame_size(playback))
        });
        if let Some(lightbox) = self.image_lightbox.as_mut() {
            if event.is_click_or_activate(chat::KEY_LIGHTBOX_ZOOM_IN) {
                lightbox.zoom_in(lightbox_image_size);
                return;
            }
            if event.is_click_or_activate(chat::KEY_LIGHTBOX_ZOOM_OUT) {
                lightbox.zoom_out(lightbox_image_size);
                return;
            }
            if event.is_click_or_activate(chat::KEY_LIGHTBOX_ZOOM_FIT) {
                lightbox.fit();
                return;
            }
            if event.is_click_or_activate(chat::KEY_LIGHTBOX_ZOOM_NATURAL) {
                lightbox.natural_size();
                return;
            }
            // Drag-to-pan on the image surface. Disabled at zoom <= 1.0
            // since there's nothing to pan to — the image is centred
            // and the body already shows everything.
            if event.route() == Some(chat::KEY_LIGHTBOX_IMAGE) && !lightbox.fit_to_window && lightbox.zoom > 1.0 {
                match event.kind {
                    UiEventKind::PointerDown => {
                        if let Some(pos) = event.pointer {
                            lightbox.drag.anchor = Some((pos, lightbox.pan));
                        }
                        return;
                    }
                    UiEventKind::Drag => {
                        if let Some((anchor_pos, start_pan)) = lightbox.drag.anchor
                            && let Some(pos) = event.pointer
                        {
                            lightbox.pan = (
                                start_pan.0 + (pos.0 - anchor_pos.0),
                                start_pan.1 + (pos.1 - anchor_pos.1),
                            );
                        }
                        return;
                    }
                    UiEventKind::PointerUp => {
                        lightbox.drag.anchor = None;
                        return;
                    }
                    _ => {}
                }
            }
        }

        // Video lightbox. Open via the file-card "Play" button on
        // a downloaded video; close via Close button, scrim, or
        // Escape. Controls (play/pause, mute, scrub) live inside
        // the panel so they only reach this handler when the
        // panel is open.
        if event.kind == UiEventKind::Click
            && let Some(route) = event.route()
            && let Some(transfer_id) = video::parse_open_video_key(route)
        {
            self.open_video_lightbox(transfer_id);
            return;
        }
        if self.active_video.is_some()
            && (event.is_click_or_activate(video::KEY_LIGHTBOX_CLOSE)
                || (event.is_route(video::KEY_LIGHTBOX_DISMISS) && event.kind == UiEventKind::Click)
                || event.kind == UiEventKind::Escape)
        {
            self.close_video_lightbox();
            return;
        }
        if let Some(active) = self.active_video.as_mut() {
            // Keyboard shortcuts: arrows (seek ±5s, +Shift =
            // ±30s), Home/End (seek to start/end), M (mute).
            // Space is handled separately via Activate routed to
            // the focused surface — aetna translates focused-
            // Space into Activate before KeyDown reaches us.
            if video::handle_lightbox_key(active, &event) {
                return;
            }
            // Click or Space/Enter on the surface itself toggles
            // play. Conventional video-player behaviour and the
            // primary path for play/pause once focus has landed
            // anywhere inside the lightbox.
            if event.is_click_or_activate(video::KEY_LIGHTBOX_SURFACE) {
                active.toggle_play();
                return;
            }
            if event.is_click_or_activate(video::KEY_PLAY_PAUSE) {
                active.toggle_play();
                return;
            }
            if event.is_click_or_activate(video::KEY_MUTE) {
                active.toggle_mute();
                return;
            }
            // Scrub bar: pointer-down anchors a drag, drag fires
            // seeks at the new value, pointer-up clears the
            // scrubbing flag so refresh_scrub_value resumes
            // tracking the playhead.
            if event.is_route(video::KEY_SCRUB)
                && let (Some(rect), Some(x)) = (event.target_rect(), event.pointer_x())
            {
                match event.kind {
                    UiEventKind::PointerDown | UiEventKind::Drag => {
                        let n = aetna_core::widgets::slider::normalized_from_event(rect, x);
                        active.scrubbing = true;
                        active.seek_normalized(n);
                        return;
                    }
                    UiEventKind::PointerUp | UiEventKind::Click => {
                        let n = aetna_core::widgets::slider::normalized_from_event(rect, x);
                        active.seek_normalized(n);
                        active.scrubbing = false;
                        return;
                    }
                    _ => {}
                }
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
        if let Some(action) = aetna_core::widgets::select::classify_event(&event, KEY_TB_VOICE_MODE) {
            use aetna_core::widgets::select::SelectAction;
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
        match room_tree::handle_event(&mut self.room_tree, &event, &room_tree_state) {
            RoomTreeOutcome::Ignored => {}
            RoomTreeOutcome::Handled => (),
            RoomTreeOutcome::Dispatch(commands) => {
                let auto_sync = self.settings.settings().chat.auto_sync_history;
                let has_join = commands.iter().any(|c| matches!(c, Command::JoinRoom { .. }));
                for cmd in &commands {
                    if let Command::JoinRoom { room_id } = cmd {
                        self.pending_self_join = Some(*room_id);
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
            tracing::warn!("rumble-aetna: connect_to_recent({idx}) — out of bounds");
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

    /// Parse the composer line. Slash commands (`/msg`, `/tree`)
    /// dispatch their dedicated `Command` variants; usage errors land
    /// in the local chat log via `Command::LocalMessage`. Plain text
    /// goes out as a `Command::SendChat` to the current room.
    fn parse_and_send_chat(&mut self, raw: &str) {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return;
        }
        if trimmed == "/msg" || trimmed.starts_with("/msg ") {
            let rest = trimmed.strip_prefix("/msg").unwrap().trim_start();
            match rest.split_once(' ') {
                Some((target_name, body)) => {
                    let body = body.trim();
                    if body.is_empty() {
                        self.backend.send(Command::LocalMessage {
                            text: "Usage: /msg <username> <message>".to_string(),
                        });
                        return;
                    }
                    let snapshot = self.backend.state();
                    let target = snapshot.users.iter().find(|u| u.username == target_name);
                    match target {
                        Some(user) => {
                            let uid = user.user_id.as_ref().map(|id| id.value).unwrap_or(0);
                            self.backend.send(Command::SendDirectMessage {
                                target_user_id: uid,
                                target_username: target_name.to_string(),
                                text: body.to_string(),
                            });
                        }
                        None => {
                            self.backend.send(Command::LocalMessage {
                                text: format!("User '{target_name}' not found"),
                            });
                        }
                    }
                }
                None => {
                    self.backend.send(Command::LocalMessage {
                        text: "Usage: /msg <username> <message>".to_string(),
                    });
                }
            }
            return;
        }
        if trimmed == "/tree" || trimmed.starts_with("/tree ") {
            let rest = trimmed.strip_prefix("/tree").unwrap().trim_start();
            if rest.is_empty() {
                self.backend.send(Command::LocalMessage {
                    text: "Usage: /tree <message>".to_string(),
                });
            } else {
                self.backend.send(Command::SendTreeChat { text: rest.to_string() });
            }
            return;
        }
        self.backend.send(Command::SendChat {
            text: trimmed.to_string(),
        });
    }

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
        let handle = self.runtime.spawn(async {
            rfd::AsyncFileDialog::new()
                .pick_file()
                .await
                .map(|f| f.path().to_path_buf())
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
        // Keep the temp dir alive for the lifetime of the share —
        // ShareFile copies the bytes into the relay stream, but the
        // file must still exist when the relay task picks it up.
        // Process exit cleans the OS temp dir; an explicit cleanup
        // belongs with the broader transfer-history work.
        std::mem::forget(temp_dir);
        self.backend.send(Command::LocalMessage {
            text: "Sharing pasted image".to_string(),
        });
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
        let handle = self.runtime.spawn(async move {
            let mut dialog = rfd::AsyncFileDialog::new();
            if let Some(start) = initial {
                dialog = dialog.set_directory(start);
            }
            dialog.pick_folder().await.map(|f| f.path().to_path_buf())
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
                self.backend.send(Command::LocalMessage {
                    text: format!("Sharing {}", path.display()),
                });
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
            if let Some(rumble_protocol::ChatAttachment::FileOffer(o)) = &m.attachment {
                if o.transfer_id == transfer_id {
                    Some(o.clone())
                } else {
                    None
                }
            } else {
                None
            }
        });
        let name = offer.map(|o| o.name).unwrap_or_else(|| transfer_id.to_string());
        let local_path = self
            .backend
            .transfers()
            .into_iter()
            .find(|t| t.id.0 == transfer_id)
            .and_then(|t| t.local_path);
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
        let handle = self.runtime.spawn(async move {
            rfd::AsyncFileDialog::new()
                .set_file_name(&name)
                .save_file()
                .await
                .map(|f| f.path().to_path_buf())
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
        let offer = snapshot
            .chat_messages
            .iter()
            .rev()
            .find_map(|m| match m.attachment.as_ref() {
                Some(rumble_protocol::ChatAttachment::FileOffer(o)) if o.transfer_id == transfer_id => Some(o.clone()),
                _ => None,
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
        if !self.image_cache.contains_key(transfer_id) {
            return;
        }
        let snapshot = self.backend.state();
        let name = snapshot
            .chat_messages
            .iter()
            .rev()
            .find_map(|m| match m.attachment.as_ref() {
                Some(rumble_protocol::ChatAttachment::FileOffer(o)) if o.transfer_id == transfer_id => {
                    Some(o.name.clone())
                }
                _ => None,
            })
            .unwrap_or_else(|| transfer_id.to_string());
        self.image_lightbox = Some(chat::Lightbox::new(transfer_id, name));

        // Force-resume an animated entry on lightbox open: the user
        // explicitly chose to view it, so the chat-settings autoplay
        // gate doesn't apply here. Static entries simply have no
        // playback record.
        if let Some(pb) = self.gif_playback.get_mut(transfer_id)
            && !pb.playing
        {
            pb.playing = true;
            pb.last_advance = Instant::now();
        }

        // Cancel any previous in-flight full-res decode and reset
        // the slot — opening a new lightbox shouldn't leak the prior
        // image or fold in a stale decode that lands later.
        if let Some(prev) = self.pending_lightbox_decode.take() {
            prev.abort();
        }
        self.lightbox_full = None;

        // Look up the local file path from the live transfer set; if
        // it's missing (e.g., the transfer plugin GC'd a finished
        // entry) the lightbox just keeps showing the thumbnail.
        let path = self
            .backend
            .transfers()
            .into_iter()
            .find(|t| t.id.0 == transfer_id)
            .and_then(|t| t.local_path);
        let Some(path) = path else {
            return;
        };
        self.pending_lightbox_decode = Some(
            self.runtime
                .spawn_blocking(move || decode_image(&path, Some(MAX_LIGHTBOX_PX))),
        );
    }

    /// Close the lightbox: clear the active state, drop the
    /// full-resolution image to free GPU memory, and abort any
    /// in-flight decode so it can't deliver a result we'd just throw
    /// away.
    fn close_lightbox(&mut self) {
        self.image_lightbox = None;
        self.lightbox_full = None;
        if let Some(handle) = self.pending_lightbox_decode.take() {
            handle.abort();
        }
    }

    /// Open the video lightbox for `transfer_id`. Looks up the
    /// downloaded file's local path from the live transfer set and
    /// kicks off `VideoStream::open` on the runtime — `load_file`
    /// blocks 10–50ms while libmpv parses the container, which
    /// would hitch the UI if done synchronously. The completed
    /// stream lands in `self.active_video` next frame via
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
        if self.active_video.as_ref().map(|a| a.transfer_id.as_str()) == Some(transfer_id) {
            return;
        }
        // Find the file's name + path from the live transfer set.
        // Bail if the transfer was GC'd between the click and the
        // event firing.
        let entry = self.backend.transfers().into_iter().find(|t| t.id.0 == transfer_id);
        let Some(status) = entry else {
            return;
        };
        let Some(path) = status.local_path.clone() else {
            return;
        };
        let name = status.name.clone();
        let id = status.id.0.clone();

        // Drop any existing active video / pending open. Newer
        // intent supersedes older.
        self.active_video = None;
        if let Some(prev) = self.pending_video_open.take() {
            prev.abort();
        }

        self.pending_video_open = Some(self.runtime.spawn_blocking(move || {
            // Open looped so the lightbox doesn't freeze the
            // moment a short clip ends — matches the "preview"
            // expectation users have for chat-attachment video.
            let stream = rumble_video::VideoStream::open(&path, true)?;
            Ok((id, name, stream))
        }));
    }

    /// Tear down the video lightbox: drop the stream (which
    /// joins the worker and terminates libmpv), drop the GPU
    /// mirror, abort any in-flight open.
    fn close_video_lightbox(&mut self) {
        self.active_video = None;
        if let Some(handle) = self.pending_video_open.take() {
            handle.abort();
        }
    }

    /// Drain a finished `VideoStream::open` task. On success,
    /// promote the stream to `active_video`; on failure, log and
    /// drop. The GPU mirror is allocated lazily in
    /// [`Self::sync_active_video_gpu`] once the host's wgpu
    /// device is available.
    fn poll_video_open(&mut self) {
        let Some(handle) = self.pending_video_open.as_ref() else {
            return;
        };
        if !handle.is_finished() {
            return;
        }
        let handle = self.pending_video_open.take().unwrap();
        let result = match self.runtime.block_on(handle) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "video: open task panicked");
                return;
            }
        };
        match result {
            Ok((id, name, stream)) => {
                self.active_video = Some(video::ActiveVideo::new(id, name, stream));
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
        if let Some(pb) = self.gif_playback.get_mut(transfer_id) {
            pb.playing = !pb.playing;
            // Reset the wall-clock anchor whenever play resumes so the
            // current frame still shows for its full delay rather than
            // immediately advancing if the entry sat paused for a while.
            if pb.playing {
                pb.last_advance = Instant::now();
            }
        }
    }

    /// Drain a finished full-resolution lightbox decode. Discard the
    /// result if the lightbox has been closed in the meantime — the
    /// user moved on, no point uploading the texture.
    fn poll_lightbox_decode(&mut self) {
        let Some(handle) = self.pending_lightbox_decode.as_ref() else {
            return;
        };
        if !handle.is_finished() {
            return;
        }
        let handle = self.pending_lightbox_decode.take().unwrap();
        if self.image_lightbox.is_none() {
            // User closed the lightbox before decode landed — drop.
            return;
        }
        match self.runtime.block_on(handle) {
            Ok(Ok(img)) => {
                self.lightbox_full = Some(img);
            }
            Ok(Err(e)) => {
                tracing::debug!("lightbox full-res decode failed: {e}");
            }
            Err(e) if e.is_cancelled() => {}
            Err(e) => {
                tracing::error!("lightbox decode task panicked: {e}");
            }
        }
    }

    /// Diff per-frame `State` against the previous frame to fire event
    /// sounds once per change: new remote chat messages (`Message`),
    /// connect/disconnect transitions (`Connect`/`Disconnect`), and
    /// remote users entering/leaving our room (`UserJoin`/`UserLeave`).
    /// The first observation of each tracked value seeds the baseline
    /// without firing, so connecting to a populated server doesn't dump
    /// a flurry of beeps.
    fn pump_sfx(&mut self) {
        let snapshot = self.backend.state();

        // New remote chat messages. Direct messages get a distinct cue.
        let count = snapshot.chat_messages.len();
        if count > self.prev_chat_count && self.prev_chat_count > 0 {
            let mut had_dm = false;
            let mut had_room = false;
            for m in snapshot.chat_messages[self.prev_chat_count..]
                .iter()
                .filter(|m| !m.is_local)
            {
                match m.kind {
                    rumble_protocol::ChatMessageKind::DirectMessage { .. } => had_dm = true,
                    _ => had_room = true,
                }
            }
            if had_dm {
                self.play_sfx(SfxKind::PrivateMessage);
            }
            if had_room {
                self.play_sfx(SfxKind::Message);
            }
        }
        self.prev_chat_count = count;

        // Connect / disconnect transitions. A kick produces a disconnect
        // with `kicked` set — play the harsher Kicked cue instead.
        let connected = snapshot.connection.is_connected();
        if connected != self.prev_connected {
            self.play_sfx(if connected {
                SfxKind::Connect
            } else if snapshot.kicked.is_some() {
                SfxKind::Kicked
            } else {
                SfxKind::Disconnect
            });
            self.prev_connected = connected;
        }

        // Server-side mute by an admin (distinct from our own self-mute).
        let server_muted = snapshot
            .my_user_id
            .and_then(|id| snapshot.get_user(id))
            .is_some_and(|u| u.server_muted);
        if server_muted && !self.prev_server_muted {
            self.play_sfx(SfxKind::ServerMute);
        }
        self.prev_server_muted = server_muted;

        // Our own room changes (SelfChannelJoin / SelfChannelMoved) and
        // remote users entering / leaving our room (UserJoin / UserLeave).
        let my_id = snapshot.my_user_id;
        let members: HashSet<u64> = match snapshot.my_room_id {
            Some(room) => snapshot
                .users_in_room(room)
                .iter()
                .filter_map(|u| u.user_id.as_ref().map(|id| id.value))
                .filter(|id| Some(*id) != my_id)
                .collect(),
            None => HashSet::new(),
        };
        if snapshot.my_room_id == self.prev_room_id {
            if members.difference(&self.prev_room_members).next().is_some() {
                self.play_sfx(SfxKind::UserJoin);
            }
            if self.prev_room_members.difference(&members).next().is_some() {
                self.play_sfx(SfxKind::UserLeave);
            }
        } else {
            // Our room id changed. Skip connect/disconnect transitions
            // (prev or current room is None) — Connect/Disconnect cover
            // those. A switch to the room we just asked to join is our own
            // action; anything else means we were relocated.
            if self.prev_room_id.is_some() && snapshot.my_room_id.is_some() {
                if snapshot.my_room_id == self.pending_self_join {
                    self.play_sfx(SfxKind::SelfChannelJoin);
                } else {
                    self.play_sfx(SfxKind::SelfChannelMoved);
                }
            }
            self.pending_self_join = None;
        }
        self.prev_room_id = snapshot.my_room_id;
        self.prev_room_members = members;

        // Drop accepted-offer ids when the connection drops so a
        // fresh session re-runs auto-download evaluation.
        if !connected && !self.auto_handled_offers.is_empty() {
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

    /// Walk the live transfer list each frame and decode any newly-
    /// completed image transfers into the chat-module image cache.
    /// Decoding runs synchronously on the UI thread because:
    ///
    /// * thumbnails are small (we cap at MAX_PREVIEW_PIXELS) and PNG /
    ///   JPEG decode at that size finishes in a millisecond or two on
    ///   modern hardware,
    /// * we only do it once per `transfer_id` per session (cached in
    ///   `image_cache`; failures are recorded in `image_failed`),
    /// * doing it asynchronously would require a separate `Arc<Mutex>`
    ///   pump and would not noticeably improve perceived latency.
    ///
    /// On disconnect the cache is cleared so a reconnect re-decodes
    /// against whatever the new session's local paths are.
    fn pump_image_cache(&mut self) {
        let snapshot = self.backend.state();
        if !snapshot.connection.is_connected() {
            return;
        }
        let transfers = self.backend.transfers();
        if transfers.is_empty() {
            return;
        }
        for status in transfers {
            if !status.is_finished {
                continue;
            }
            let Some(path) = status.local_path.as_ref() else {
                continue;
            };
            let id = &status.id.0;
            if self.image_cache.contains_key(id) || self.image_failed.contains(id) {
                continue;
            }
            if !chat::is_image_name(&status.name) {
                // Mark as "not an image" so we don't re-check the
                // extension every frame.
                self.image_failed.insert(id.clone());
                continue;
            }
            match decode_thumbnail(path) {
                Ok(cached) => {
                    if cached.is_animated() {
                        // Seed playback state once so the inline preview
                        // and lightbox observe a stable playhead. Honour
                        // the chat-settings autoplay default for the
                        // initial state; play/pause clicks toggle from
                        // there.
                        let autoplay = self.settings.settings().chat.gif_autoplay;
                        self.gif_playback
                            .entry(id.clone())
                            .or_insert_with(|| chat::GifPlayback::new(autoplay));
                    }
                    self.image_cache.insert(id.clone(), cached);
                }
                Err(e) => {
                    tracing::debug!(
                        "image preview decode failed for {} ({}): {e}",
                        status.name,
                        path.display()
                    );
                    self.image_failed.insert(id.clone());
                }
            }
        }
    }

    /// Mirrors [`Self::pump_image_cache`] but for video files:
    /// kicks off a one-shot libmpv decode of the first frame for
    /// each downloaded video that doesn't already have a thumbnail
    /// (or a record of failure), and drains finished decode tasks
    /// into [`Self::video_thumbs`].
    ///
    /// Decode happens off-thread via `runtime.spawn_blocking` so
    /// the 50–200ms libmpv startup cost doesn't stall the UI on
    /// the frame the file finishes downloading.
    fn pump_video_thumbs(&mut self) {
        let snapshot = self.backend.state();
        if !snapshot.connection.is_connected() {
            // Drain any in-flight decodes whether or not we're
            // still connected — they're cheap to land and the
            // user might reconnect mid-decode.
            self.drain_finished_video_thumbs();
            return;
        }

        // Drain finished tasks first so the spawn loop below
        // doesn't see stale `pending_video_thumbs` entries for
        // ids that just landed.
        self.drain_finished_video_thumbs();

        for status in self.backend.transfers() {
            if !status.is_finished {
                continue;
            }
            let Some(path) = status.local_path.clone() else {
                continue;
            };
            let id = status.id.0.clone();
            if self.video_thumbs.contains_key(&id)
                || self.failed_video_thumbs.contains(&id)
                || self.pending_video_thumbs.contains_key(&id)
            {
                continue;
            }
            if !video::is_video_name(&status.name) {
                // Not a video — leave the file card alone. We
                // don't mark it failed because there's nothing
                // to retry.
                continue;
            }
            let handle = self.runtime.spawn_blocking(move || video::extract_thumbnail(&path));
            self.pending_video_thumbs.insert(id, handle);
        }
    }

    /// Drain every finished entry in [`Self::pending_video_thumbs`]
    /// into either `video_thumbs` (success) or `failed_video_thumbs`
    /// (error). Pulled out of `pump_video_thumbs` so the early-
    /// return when disconnected still flushes results.
    fn drain_finished_video_thumbs(&mut self) {
        let finished: Vec<String> = self
            .pending_video_thumbs
            .iter()
            .filter(|(_, h)| h.is_finished())
            .map(|(id, _)| id.clone())
            .collect();
        for id in finished {
            let handle = self.pending_video_thumbs.remove(&id).unwrap();
            match self.runtime.block_on(handle) {
                Ok(Ok(image)) => {
                    self.video_thumbs.insert(id, image);
                }
                Ok(Err(e)) => {
                    tracing::debug!("video thumbnail decode failed for {id}: {e}");
                    self.failed_video_thumbs.insert(id);
                }
                Err(join_err) => {
                    tracing::warn!("video thumbnail decode task panicked for {id}: {join_err}");
                    self.failed_video_thumbs.insert(id);
                }
            }
        }
    }

    /// Advance every active GIF playhead by the elapsed wall-clock
    /// time. Each frame's display delay is treated as the dwell time
    /// before the next frame appears — multi-frame skips are folded
    /// into a single update so a tab the user backgrounded for a few
    /// seconds resumes at the right frame instead of catching up frame
    /// by frame.
    ///
    /// Drops `gif_playback` entries whose underlying cache entry has
    /// gone away (e.g. because the transfer was evicted), so the map
    /// can't outgrow the cache it shadows.
    fn pump_gif_animations(&mut self) {
        let now = Instant::now();
        // Walk by key first so the borrow on `image_cache` is dropped
        // before we mutate playback. Avoids a double-borrow on `self`.
        let keys: Vec<String> = self.gif_playback.keys().cloned().collect();
        for id in keys {
            let drop_entry = match self.image_cache.get(&id) {
                Some(chat::CachedImage::Animated { frames, .. }) => {
                    if let Some(pb) = self.gif_playback.get_mut(&id) {
                        if !pb.playing || frames.is_empty() {
                            // Paused: keep the entry, don't burn cycles.
                            // Empty frames is impossible by construction
                            // (decode collapses to Static) but guard
                            // anyway so the loop can't divide by zero.
                            false
                        } else {
                            advance_gif_frame(pb, frames, now);
                            false
                        }
                    } else {
                        false
                    }
                }
                // Cache entry vanished or is Static — drop the playback
                // record so the map stays bounded.
                _ => true,
            };
            if drop_entry {
                self.gif_playback.remove(&id);
            }
        }
    }

    /// Lazily allocate per-message wgpu textures for animated entries
    /// with active playback, upload the current frame to each, and
    /// drop GPU mirrors whose playback has gone away. Called from
    /// [`WinitWgpuApp::before_paint`] so the upload lands before
    /// aetna's paint pass samples the texture this frame.
    ///
    /// Frame-advance bookkeeping lives in [`Self::pump_gif_animations`]
    /// (run from `before_build`); this method is purely the GPU-side
    /// mirror.
    fn sync_animated_gpu(&mut self, queue: &wgpu::Queue) {
        let Some(device) = self.gpu_device.as_ref() else {
            // gpu_setup hasn't run yet (or we're under a test backend
            // that never calls it). Nothing to do.
            return;
        };

        // Drop mirrors whose playback record is gone — keeps the
        // GPU resident set bounded by the CPU-side cache.
        self.animated_gpu.retain(|id, _| self.gif_playback.contains_key(id));

        for (id, pb) in &self.gif_playback {
            let Some(chat::CachedImage::Animated { frames }) = self.image_cache.get(id) else {
                continue;
            };
            if frames.is_empty() {
                continue;
            }
            let idx = pb.frame_idx.min(frames.len() - 1);
            let entry = self
                .animated_gpu
                .entry(id.clone())
                .or_insert_with(|| AnimatedGpu::allocate(device, &frames[0].0));
            entry.upload_frame(queue, frames, idx);
        }
    }

    /// Lazily allocate the active video's GPU mirror (needs the
    /// wgpu device, which arrives after the stream itself), then
    /// upload the latest decoded frame if the worker has produced
    /// one since last paint.
    fn sync_active_video_gpu(&mut self, queue: &wgpu::Queue) {
        let Some(device) = self.gpu_device.as_ref() else {
            return;
        };
        let Some(active) = self.active_video.as_mut() else {
            return;
        };
        if active.gpu.is_none() {
            active.gpu = Some(rumble_video::VideoGpu::allocate(device, &active.stream));
        }
        if let Some(gpu) = active.gpu.as_mut() {
            gpu.upload_if_changed(queue, &active.stream);
        }
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

    /// Route a [`SettingsOutcome`] back into the App. Returns `true`
    /// when the outcome consumed the originating event, so the parent
    /// handler can short-circuit.
    fn dispatch_settings_outcome(&mut self, outcome: SettingsOutcome) -> bool {
        match outcome {
            SettingsOutcome::Ignored => false,
            SettingsOutcome::Handled => true,
            SettingsOutcome::Close => {
                self.settings_state.close();
                true
            }
            SettingsOutcome::OpenIdentityWizard => {
                self.settings_state.close();
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
                self.wizard = WizardState::ConnectingAgent;
                self.spawn_connect_op();
                true
            }
            SettingsOutcome::OpenElevate => {
                self.settings_state.close();
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
    /// for a non-blank user.
    pub fn set_local_identity_for_test(&mut self) {
        let _ = self.identity.generate_local_key(None);
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
    /// runtime path goes through `pump_image_cache`, but bundle dumps
    /// don't have a real file-transfer plugin behind them — so let the
    /// scene-builder push a synthetic decoded image directly.
    pub fn insert_image_preview_for_test(&mut self, transfer_id: impl Into<String>, img: Image) {
        self.image_cache
            .insert(transfer_id.into(), chat::CachedImage::Static(img));
    }

    /// Test/scene-dump hook to open the lightbox without a click event.
    /// Mirrors `open_lightbox` but skips the chat-history lookup so the
    /// scene can target a synthetic offer directly.
    pub fn open_lightbox_for_test(&mut self, transfer_id: impl Into<String>, name: impl Into<String>) {
        self.image_lightbox = Some(chat::Lightbox::new(transfer_id, name));
    }

    /// Test/scene-dump hook to set the lightbox's zoom + pan directly,
    /// for snapshotting the controls in non-default state. Caller is
    /// responsible for setting them to plausible values; the runtime
    /// path goes through `Lightbox::set_zoom` / drag handlers and so
    /// always stays in range.
    pub fn set_lightbox_zoom_for_test(&mut self, zoom: f32, pan: (f32, f32)) {
        if let Some(lb) = self.image_lightbox.as_mut() {
            lb.fit_to_window = false;
            lb.zoom = zoom;
            lb.pan = pan;
        }
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

fn top_toolbar(state: &State) -> El {
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
    use aetna_core::widgets::popover::{Anchor, menu_item, popover, popover_panel};
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

    overlay([
        El::new(Kind::Custom("drop-target-scrim"))
            .fill(tokens::OVERLAY_SCRIM)
            .fill_size(),
        stack([banner])
            .fill_size()
            .padding(Sides::all(tokens::SPACE_7))
            .align(Align::Center),
    ])
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
