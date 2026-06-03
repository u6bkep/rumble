//! Centralized, event-driven store for transfer-keyed media state.
//!
//! Replaces what used to be eight parallel maps on [`crate::RumbleApp`]
//! plus three per-frame pumps that walked `backend.transfers()` to find
//! work. The bug class that motivated the consolidation (see
//! `docs/plugin-attachment-redesign.md`) was a renderer hiding a
//! failed transfer behind a thumbnail because two of those maps
//! disagreed about what "interesting" meant.
//!
//! ## Inputs
//!
//! - [`MediaCache::on_event`] consumes
//!   [`rumble_client::BackendEvent::TransferStageChanged`] forwarded
//!   by the App's fan-out. Terminal transitions drive everything: a
//!   transfer hitting `Done` is what kicks off image/video decoding,
//!   a transfer hitting `Failed` clears any prior retry state. The
//!   `Active` and `Paused` events are accepted but ignored — they
//!   would only matter if we wanted to evict caches on resume, which
//!   we don't.
//! - [`MediaCache::drain_pending`] is called once per frame from
//!   `App::before_build` to land any off-thread decode results that
//!   completed since last frame (video thumbs, lightbox full-res
//!   decode). These aren't event-driven because they're tokio
//!   `JoinHandle` completions, not transfer transitions.
//! - [`MediaCache::advance_gif_playheads`] is also per-frame but
//!   wall-clock-driven: each animated entry's playhead advances by
//!   the elapsed time since last frame. Damascene's `redraw_within`
//!   schedules the wake-ups so off-screen GIFs don't burn cycles.
//!
//! ## Outputs
//!
//! Lookup accessors (`image_for`, `gif_playback_for`,
//! `animated_gpu_for`, `video_thumb_for`, `lightbox_image_for`)
//! return `Option<&T>` keyed by `transfer_id`. The chat renderer
//! takes a `&MediaCache` and pulls what it needs per message.
//!
//! ## Plugin-agnostic by construction
//!
//! `MediaCache` only ever sees `(transfer_id, local_path, name)`
//! pulled from `TransferStage::Done`. Whether the bytes arrived via
//! the relay plugin, a future torrent plugin, or anything else makes
//! no difference: a file is a file, and the image/video decoders
//! don't care how it got there.

use std::{
    collections::{HashMap, HashSet},
    io::BufReader,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use damascene_core::prelude::*;
use rumble_client::BackendEvent;
use rumble_client_traits::TransferStage;
use tokio::{runtime::Handle, task::JoinHandle};

use crate::{
    animated_gpu::AnimatedGpu,
    chat,
    model::{self, LoadedModel, ModelError, ModelThumbnailer},
};

/// Cap on the longest edge of an inline chat thumbnail, in pixels.
/// ~1024 keeps the 400px-tall preview rect sharp on a 2× display
/// without burning RAM on phone-camera-sized sources.
const MAX_PREVIEW_PX: u32 = 1024;

/// Cap on the longest edge of a lightbox full-resolution decode, in
/// pixels. Generous enough that real-world photos go through
/// untouched but defensive against pathological inputs (and well
/// under the typical 8192 GPU texture limit).
const MAX_LIGHTBOX_PX: u32 = 4096;

/// Max attempts before giving up on a video thumbnail. Three tries
/// covers the common transient cases (libmpv cold-start timeout, brief
/// file-contention with an in-flight upload reader) without churning
/// libmpv forever on files that genuinely won't decode.
const THUMB_MAX_ATTEMPTS: u8 = 3;

/// Per-attempt delay schedule between video-thumbnail retries.
/// Indexed by completed-attempt count: after the 1st failure wait 2s,
/// after the 2nd wait 8s. Long enough to outlast a busy upload reader
/// while staying interactive on the cold-start path.
const THUMB_RETRY_DELAYS: &[Duration] = &[Duration::from_secs(2), Duration::from_secs(8)];

/// Per-id retry bookkeeping for video thumbnail decoding.
#[derive(Clone, Copy, Debug)]
struct ThumbFailure {
    attempts: u8,
    last_attempt: Instant,
}

/// Per-id state for an in-flight full-resolution lightbox decode.
struct PendingLightboxDecode {
    transfer_id: String,
    handle: JoinHandle<Result<chat::CachedImage, image::ImageError>>,
}

/// Centralized media cache. See module docs for the lifecycle model.
pub struct MediaCache {
    runtime: Handle,

    /// Defaults read from the chat settings. Updated each frame via
    /// [`Self::set_gif_autoplay_default`] so a Done event landing
    /// later in the session uses the user's current preference.
    gif_autoplay_default: bool,

    image_cache: HashMap<String, chat::CachedImage>,
    /// Transfer ids we have tried to decode and failed (corrupt,
    /// truncated, unsupported codec) — recorded so we don't keep
    /// re-attempting on every subsequent Done event for the same id
    /// (won't happen with stable UUIDs but cheap insurance).
    image_failed: HashSet<String>,

    gif_playback: HashMap<String, chat::GifPlayback>,
    animated_gpu: HashMap<String, AnimatedGpu>,

    video_thumbs: HashMap<String, Image>,
    failed_video_thumbs: HashMap<String, ThumbFailure>,
    pending_video_thumbs: HashMap<String, JoinHandle<Result<Image, rumble_video::Error>>>,
    /// Pending video-thumb decodes — when a transfer hits Done we
    /// queue it here and `drain_pending` retries with backoff on
    /// failure. Separate from `pending_video_thumbs` because retry
    /// scheduling needs a wall-clock-driven re-spawn that's not
    /// gated on an event arriving.
    pending_video_thumb_retries: HashMap<String, (PathBuf, String)>,

    /// Parsed 3D-model geometry keyed by `transfer_id`, kept so the
    /// lightbox can project a live `chart3d` from the same upload the
    /// thumbnail rendered. Cloning a [`LoadedModel`] is a cheap `Arc`
    /// bump on the geometry handle.
    models: HashMap<String, LoadedModel>,
    /// Offscreen-rendered model poster thumbnails, keyed by `transfer_id`
    /// (the 3D analogue of [`Self::video_thumbs`]). Populated by
    /// [`Self::sync_model_thumbs`] once a model has been parsed and the
    /// host's wgpu device is available.
    model_thumbs: HashMap<String, Image>,
    /// Transfer ids whose model parse failed — recorded so a repeat Done
    /// event doesn't re-spawn a doomed parse.
    model_failed: HashSet<String>,
    /// In-flight off-thread model parses, keyed by `transfer_id`.
    pending_model_loads: HashMap<String, JoinHandle<Result<LoadedModel, ModelError>>>,
    /// Renders model posters offscreen. Lazily builds its `Runner` on the
    /// first thumbnail; reused across models.
    model_thumbnailer: ModelThumbnailer,

    /// Active full-res lightbox decode. Only one at a time — opening
    /// a new lightbox aborts the prior decode.
    pending_lightbox: Option<PendingLightboxDecode>,
    /// Decoded full-resolution image for the currently-open lightbox,
    /// keyed by `transfer_id` so a re-open of the same image avoids
    /// re-decoding while the cache entry is live.
    lightbox_full: HashMap<String, chat::CachedImage>,
}

impl MediaCache {
    pub fn new(runtime: Handle, gif_autoplay_default: bool) -> Self {
        Self {
            runtime,
            gif_autoplay_default,
            image_cache: HashMap::new(),
            image_failed: HashSet::new(),
            gif_playback: HashMap::new(),
            animated_gpu: HashMap::new(),
            video_thumbs: HashMap::new(),
            failed_video_thumbs: HashMap::new(),
            pending_video_thumbs: HashMap::new(),
            pending_video_thumb_retries: HashMap::new(),
            models: HashMap::new(),
            model_thumbs: HashMap::new(),
            model_failed: HashSet::new(),
            pending_model_loads: HashMap::new(),
            model_thumbnailer: ModelThumbnailer::new(),
            pending_lightbox: None,
            lightbox_full: HashMap::new(),
        }
    }

    /// Update the autoplay default. The App calls this once per
    /// frame from settings so a Done event landing later in the
    /// session uses the user's current preference rather than the
    /// value at MediaCache construction.
    pub fn set_gif_autoplay_default(&mut self, autoplay: bool) {
        self.gif_autoplay_default = autoplay;
    }

    /// React to a backend event. Non-transfer events are silently
    /// ignored so the App's fan-out can pass everything through.
    pub fn on_event(&mut self, event: &BackendEvent) {
        let BackendEvent::TransferStageChanged { id, name, stage, .. } = event else {
            return;
        };
        match stage {
            TransferStage::Done { local_path } => {
                self.on_transfer_done(&id.0, name, local_path);
            }
            TransferStage::Failed { .. } => {
                self.on_transfer_failed(&id.0);
            }
            TransferStage::Active { .. } | TransferStage::Paused { .. } => {
                // No cache reaction. Progress bars read TransferStatus
                // directly via the snapshot.
            }
        }
    }

    /// Drive on a transfer that just landed at `local_path`. Inline
    /// image decode is intentional (PNG/JPEG at MAX_PREVIEW_PX
    /// finish in single-digit ms on modern hardware, and a single
    /// per-transfer cost). Video thumbs go async — libmpv cold-start
    /// is 50–200ms.
    fn on_transfer_done(&mut self, id: &str, name: &str, local_path: &Path) {
        if chat::is_image_name(name) && !self.image_cache.contains_key(id) && !self.image_failed.contains(id) {
            match decode_image(local_path, Some(MAX_PREVIEW_PX)) {
                Ok(cached) => {
                    if cached.is_animated() {
                        self.gif_playback
                            .entry(id.to_string())
                            .or_insert_with(|| chat::GifPlayback::new(self.gif_autoplay_default));
                    }
                    self.image_cache.insert(id.to_string(), cached);
                }
                Err(e) => {
                    tracing::debug!("image preview decode failed for {name} ({}): {e}", local_path.display());
                    self.image_failed.insert(id.to_string());
                }
            }
        }
        if crate::video::is_video_name(name) && !self.video_thumbs.contains_key(id) {
            // Drop any prior failure record so the retry counter
            // starts fresh (events are transitions, so seeing Done
            // again means a successful upload after a transient
            // failure — give it another shot).
            self.failed_video_thumbs.remove(id);
            self.spawn_video_thumb(id.to_string(), local_path.to_path_buf());
            // Record the path so drain_pending can re-spawn on
            // retry without needing a fresh event.
            self.pending_video_thumb_retries
                .insert(id.to_string(), (local_path.to_path_buf(), name.to_string()));
        }
        if model::is_model_name(name)
            && !self.models.contains_key(id)
            && !self.model_failed.contains(id)
            && !self.pending_model_loads.contains_key(id)
        {
            // Parse off-thread — large STL/PLY meshes take real CPU time,
            // same rationale as the video-thumbnail extract.
            let path = local_path.to_path_buf();
            let handle = self.runtime.spawn_blocking(move || model::load_model(&path));
            self.pending_model_loads.insert(id.to_string(), handle);
        }
    }

    /// Drive on a transfer that just terminated with `Failed`. Nothing
    /// to evict from the *successful* caches (a failed transfer's
    /// path was never decoded), but cancel any in-flight decode that
    /// somehow raced in. Belt-and-suspenders against future code
    /// paths that might fire Done→Failed in quick succession.
    fn on_transfer_failed(&mut self, id: &str) {
        if let Some(handle) = self.pending_video_thumbs.remove(id) {
            handle.abort();
        }
        self.pending_video_thumb_retries.remove(id);
        if let Some(handle) = self.pending_model_loads.remove(id) {
            handle.abort();
        }
    }

    fn spawn_video_thumb(&mut self, id: String, path: PathBuf) {
        let attempt = self.failed_video_thumbs.get(&id).map(|f| f.attempts + 1).unwrap_or(1);
        tracing::debug!(
            "media_cache: spawn video thumb extract for {id} attempt={attempt}/{THUMB_MAX_ATTEMPTS} path={}",
            path.display(),
        );
        let handle = self
            .runtime
            .spawn_blocking(move || crate::video::extract_thumbnail(&path));
        self.pending_video_thumbs.insert(id.clone(), handle);
        if let Some(fail) = self.failed_video_thumbs.get_mut(&id) {
            fail.last_attempt = Instant::now();
        }
    }

    /// Land any off-thread decode results that finished since the
    /// last frame, plus respawn any video-thumb retries whose backoff
    /// has elapsed. Called once per frame from `App::before_build`.
    pub fn drain_pending(&mut self) {
        self.drain_video_thumbs();
        self.maybe_respawn_video_thumbs();
        self.drain_model_loads();
        self.drain_lightbox();
    }

    /// Land finished off-thread model parses. On success the geometry goes
    /// into `models` (the thumbnail render happens later, in
    /// [`Self::sync_model_thumbs`], when the wgpu device is available); on
    /// failure the id is recorded so we don't re-attempt.
    fn drain_model_loads(&mut self) {
        let finished: Vec<String> = self
            .pending_model_loads
            .iter()
            .filter(|(_, h)| h.is_finished())
            .map(|(id, _)| id.clone())
            .collect();
        for id in finished {
            let handle = self.pending_model_loads.remove(&id).unwrap();
            match self.runtime.block_on(handle) {
                Ok(Ok(model)) => {
                    self.models.insert(id, model);
                }
                Ok(Err(e)) => {
                    tracing::debug!("model parse failed for {id}: {e}");
                    self.model_failed.insert(id);
                }
                Err(join_err) => {
                    tracing::warn!("model parse task panicked for {id}: {join_err}");
                    self.model_failed.insert(id);
                }
            }
        }
    }

    fn drain_video_thumbs(&mut self) {
        let finished: Vec<String> = self
            .pending_video_thumbs
            .iter()
            .filter(|(_, h)| h.is_finished())
            .map(|(id, _)| id.clone())
            .collect();
        let now = Instant::now();
        for id in finished {
            let handle = self.pending_video_thumbs.remove(&id).unwrap();
            match self.runtime.block_on(handle) {
                Ok(Ok(image)) => {
                    tracing::debug!("video thumbnail decoded for {id}: {}x{}", image.width(), image.height());
                    self.failed_video_thumbs.remove(&id);
                    self.pending_video_thumb_retries.remove(&id);
                    self.video_thumbs.insert(id, image);
                }
                Ok(Err(e)) => {
                    let entry = self.failed_video_thumbs.entry(id.clone()).or_insert(ThumbFailure {
                        attempts: 0,
                        last_attempt: now,
                    });
                    entry.attempts = entry.attempts.saturating_add(1);
                    entry.last_attempt = now;
                    if entry.attempts >= THUMB_MAX_ATTEMPTS {
                        tracing::warn!(
                            "video thumbnail decode failed for {id} (attempt {}/{THUMB_MAX_ATTEMPTS}, giving up): {e}",
                            entry.attempts,
                        );
                        self.pending_video_thumb_retries.remove(&id);
                    } else {
                        tracing::debug!(
                            "video thumbnail decode failed for {id} (attempt {}/{THUMB_MAX_ATTEMPTS}, will retry): {e}",
                            entry.attempts,
                        );
                    }
                }
                Err(join_err) => {
                    tracing::warn!("video thumbnail decode task panicked for {id}: {join_err}");
                    self.failed_video_thumbs.insert(
                        id.clone(),
                        ThumbFailure {
                            attempts: THUMB_MAX_ATTEMPTS,
                            last_attempt: now,
                        },
                    );
                    self.pending_video_thumb_retries.remove(&id);
                }
            }
        }
    }

    fn maybe_respawn_video_thumbs(&mut self) {
        let now = Instant::now();
        let ready: Vec<(String, PathBuf)> = self
            .pending_video_thumb_retries
            .iter()
            .filter_map(|(id, (path, _name))| {
                if self.video_thumbs.contains_key(id) || self.pending_video_thumbs.contains_key(id) {
                    return None;
                }
                let fail = self.failed_video_thumbs.get(id)?;
                if fail.attempts >= THUMB_MAX_ATTEMPTS {
                    return None;
                }
                let idx = (fail.attempts as usize)
                    .saturating_sub(1)
                    .min(THUMB_RETRY_DELAYS.len() - 1);
                let delay = THUMB_RETRY_DELAYS[idx];
                (now.saturating_duration_since(fail.last_attempt) >= delay).then(|| (id.clone(), path.clone()))
            })
            .collect();
        for (id, path) in ready {
            self.spawn_video_thumb(id, path);
        }
    }

    fn drain_lightbox(&mut self) {
        let Some(pending) = self.pending_lightbox.as_ref() else {
            return;
        };
        if !pending.handle.is_finished() {
            return;
        }
        let pending = self.pending_lightbox.take().unwrap();
        match self.runtime.block_on(pending.handle) {
            Ok(Ok(img)) => {
                self.lightbox_full.insert(pending.transfer_id, img);
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

    /// Advance every active GIF playhead by the elapsed wall-clock
    /// time since the last call. Multi-frame skips fold into a
    /// single update so a tab backgrounded for a few seconds resumes
    /// at the right frame instead of catching up one tick at a time.
    pub fn advance_gif_playheads(&mut self, now: Instant) {
        let keys: Vec<String> = self.gif_playback.keys().cloned().collect();
        for id in keys {
            let drop_entry = match self.image_cache.get(&id) {
                Some(chat::CachedImage::Animated { frames }) => {
                    if let Some(pb) = self.gif_playback.get_mut(&id) {
                        if !pb.playing || frames.is_empty() {
                            false
                        } else {
                            advance_gif_frame(pb, frames, now);
                            false
                        }
                    } else {
                        false
                    }
                }
                _ => true,
            };
            if drop_entry {
                self.gif_playback.remove(&id);
            }
        }
    }

    /// Sync GPU mirrors for animated entries with active playback.
    /// Called from `App::before_paint` once the wgpu device is
    /// available — that's the only time we can allocate textures.
    /// Drops mirrors whose playback record has gone away.
    pub fn sync_animated_gpu(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
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

    /// Render a poster thumbnail for any parsed model that doesn't have one
    /// yet. Called from `App::before_paint` once the wgpu device is
    /// available (the offscreen render needs it). Each model renders once;
    /// a model whose render momentarily fails (GPU readback hiccup) stays
    /// un-thumbnailed and is retried next frame.
    pub fn sync_model_thumbs(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        let pending: Vec<String> = self
            .models
            .keys()
            .filter(|id| !self.model_thumbs.contains_key(*id))
            .cloned()
            .collect();
        for id in pending {
            let Some(model) = self.models.get(&id) else {
                continue;
            };
            if let Some(image) = self.model_thumbnailer.render(device, queue, model) {
                tracing::debug!(
                    "model thumbnail rendered for {id}: {}x{}",
                    image.width(),
                    image.height()
                );
                self.model_thumbs.insert(id, image);
            }
        }
    }

    // ---------- lightbox control ----------

    /// Kick off a full-resolution decode for a freshly-opened
    /// lightbox. Aborts any prior in-flight decode (newer intent
    /// supersedes older), clears any stale full-res entry for the
    /// same id so the lightbox shows the thumbnail until the new
    /// decode lands.
    pub fn open_lightbox_decode(&mut self, transfer_id: &str, path: PathBuf) {
        if let Some(prev) = self.pending_lightbox.take() {
            prev.handle.abort();
        }
        self.lightbox_full.remove(transfer_id);
        let handle = self
            .runtime
            .spawn_blocking(move || decode_image(&path, Some(MAX_LIGHTBOX_PX)));
        self.pending_lightbox = Some(PendingLightboxDecode {
            transfer_id: transfer_id.to_string(),
            handle,
        });
    }

    /// Drop the active lightbox decode + any cached full-res image.
    /// Frees GPU memory and prevents a late-landing decode from
    /// re-populating the cache after the user closed the overlay.
    pub fn close_lightbox(&mut self) {
        self.lightbox_full.clear();
        if let Some(pending) = self.pending_lightbox.take() {
            pending.handle.abort();
        }
    }

    /// Force-resume an animated entry on lightbox open: the user
    /// explicitly chose to view it, so the chat-settings autoplay
    /// gate doesn't apply here. No-op for static entries.
    pub fn force_resume_playback(&mut self, transfer_id: &str) {
        if let Some(pb) = self.gif_playback.get_mut(transfer_id)
            && !pb.playing
        {
            pb.playing = true;
            pb.last_advance = Instant::now();
        }
    }

    /// Toggle play/pause for an animated entry. Returns the new
    /// `playing` state, or `None` if there's no entry for that id.
    pub fn toggle_playback(&mut self, transfer_id: &str) -> Option<bool> {
        let pb = self.gif_playback.get_mut(transfer_id)?;
        pb.playing = !pb.playing;
        pb.last_advance = Instant::now();
        Some(pb.playing)
    }

    // ---------- lookup accessors ----------

    pub fn image_for(&self, transfer_id: &str) -> Option<&chat::CachedImage> {
        self.image_cache.get(transfer_id)
    }

    pub fn gif_playback_for(&self, transfer_id: &str) -> Option<&chat::GifPlayback> {
        self.gif_playback.get(transfer_id)
    }

    pub fn animated_gpu_for(&self, transfer_id: &str) -> Option<&AnimatedGpu> {
        self.animated_gpu.get(transfer_id)
    }

    pub fn video_thumb_for(&self, transfer_id: &str) -> Option<&Image> {
        self.video_thumbs.get(transfer_id)
    }

    /// Poster thumbnail for a downloaded 3D model, if one has rendered.
    pub fn model_thumb_for(&self, transfer_id: &str) -> Option<&Image> {
        self.model_thumbs.get(transfer_id)
    }

    /// Parsed geometry for a downloaded 3D model, if the parse succeeded.
    /// The lightbox clones this (cheap `Arc` bump) to build its scene.
    pub fn model_for(&self, transfer_id: &str) -> Option<&LoadedModel> {
        self.models.get(transfer_id)
    }

    /// Best image available for the open lightbox: full-resolution if
    /// the decode has landed, otherwise the inline thumbnail.
    pub fn lightbox_image_for(&self, transfer_id: &str) -> Option<&chat::CachedImage> {
        self.lightbox_full
            .get(transfer_id)
            .or_else(|| self.image_cache.get(transfer_id))
    }

    // ---------- test/bundle-dump helpers ----------

    /// Mutable image-cache access for `dump_bundles` (which pre-seeds
    /// entries without going through the event pipeline). Not for
    /// runtime use; production code observes events.
    pub fn image_cache_mut(&mut self) -> &mut HashMap<String, chat::CachedImage> {
        &mut self.image_cache
    }

    /// Mutable GIF-playback access for `dump_bundles`. See
    /// [`Self::image_cache_mut`].
    pub fn gif_playback_mut(&mut self) -> &mut HashMap<String, chat::GifPlayback> {
        &mut self.gif_playback
    }

    /// Mutable video-thumb access for `dump_bundles`. See
    /// [`Self::image_cache_mut`].
    pub fn video_thumbs_mut(&mut self) -> &mut HashMap<String, Image> {
        &mut self.video_thumbs
    }

    /// Mutable model-thumb access for `dump_bundles` (seeds a poster
    /// without a GPU render). See [`Self::image_cache_mut`].
    pub fn model_thumbs_mut(&mut self) -> &mut HashMap<String, Image> {
        &mut self.model_thumbs
    }

    /// Mutable parsed-model access for `dump_bundles` (seeds geometry for
    /// the lightbox scene). See [`Self::image_cache_mut`].
    pub fn models_mut(&mut self) -> &mut HashMap<String, LoadedModel> {
        &mut self.models
    }
}

// ---------- free helpers (decoders + animation timing) ----------

/// Decode `path` into a [`chat::CachedImage`]. `max_px` caps the
/// longest edge of every emitted frame — `Some(n)` downsamples,
/// `None` loads the source at its natural resolution.
///
/// Animated GIF, animated WebP, and APNG decode every frame and
/// produce [`chat::CachedImage::Animated`]; single-frame animations
/// and all other formats collapse to `Static`. SVG goes through
/// damascene's vector parser.
fn decode_image(path: &Path, max_px: Option<u32>) -> Result<chat::CachedImage, image::ImageError> {
    use image::{
        AnimationDecoder, ImageFormat,
        codecs::{gif::GifDecoder, png::PngDecoder, webp::WebPDecoder},
    };

    // SVG isn't a raster format — `image::ImageReader` doesn't know
    // it. Detect by extension, then hand to damascene's vector parser.
    if path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("svg"))
        .unwrap_or(false)
    {
        let svg_text = std::fs::read_to_string(path).map_err(image::ImageError::IoError)?;
        let icon = damascene_core::SvgIcon::parse(&svg_text).map_err(|e| {
            image::ImageError::Decoding(image::error::DecodingError::new(
                image::error::ImageFormatHint::Name("svg".into()),
                e.to_string(),
            ))
        })?;
        return Ok(chat::CachedImage::Svg(icon));
    }

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

/// Drain an animation decoder's frames, downsample each by `max_px`,
/// pack into [`chat::CachedImage::Animated`]. A 20ms floor matches
/// browsers' behaviour on pathologically-tiny per-frame delays.
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
        return Ok(chat::CachedImage::Static(
            out.into_iter()
                .next()
                .map(|(img, _)| img)
                .unwrap_or_else(|| Image::from_rgba8(1, 1, vec![0, 0, 0, 0])),
        ));
    }

    Ok(chat::CachedImage::Animated { frames: out })
}

/// Advance one animated entry's playhead by the wall-clock time
/// elapsed since its last advance. Skips multiple frames in one
/// update if the elapsed time covers them, so resuming from
/// background doesn't fast-forward visibly.
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
        // Anchor `last_advance` at the boundary we landed on, so
        // residual sub-frame elapsed time doesn't accumulate drift.
        pb.last_advance = now - elapsed;
    }
}
