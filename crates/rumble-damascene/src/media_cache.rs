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
//! return `Option<&T>` keyed by `transfer_id`; a hit bumps the
//! entry's LRU stamp and a miss records re-decode demand, so they are
//! for genuinely-visible uses only: the lightbox, and the chat
//! history's `build_row` closures — which the virtual list invokes
//! solely for rows in the visible window, locking the shared
//! `Arc<Mutex<MediaCache>>` for the lookup. That realization-time
//! lookup is what scopes the byte-budget eviction (issue #37) to
//! what's truly on screen (issue #16): off-screen backlog media ages
//! out, and an evicted entry re-decodes from disk when its row
//! scrolls back into view.
//!
//! Because the cache is shared into `Send + Sync` row closures, the
//! whole struct must stay `Send` — asserted statically below.
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
    sync::{Arc, Mutex},
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

/// Maximum number of frames decoded from one animated image (GIF, APNG,
/// animated WebP). Animations that exceed this are truncated to the
/// frames collected so far and play as a shorter loop. A 256-frame cap
/// covers essentially all real GIFs while preventing a pathological
/// many-frame file from allocating unboundedly.
const MAX_ANIMATED_FRAMES: usize = 256;

/// Maximum total RGBA bytes accumulated across all frames of one animated
/// image, measured at source resolution *before* the thumbnail downscale.
/// Limiting the pre-thumbnail budget bounds peak RSS during decode.
/// 256 MiB accommodates a 1024×1024 animation at the full frame cap
/// (256 frames × 4 MiB/frame = 1 GiB at natural res, capped here at 256 MiB
/// so a moderate-resolution many-frame file still triggers the guard).
const MAX_ANIMATED_RGBA_BYTES: usize = 256 * 1024 * 1024;

/// Per-attempt delay schedule between video-thumbnail retries.
/// Indexed by completed-attempt count: after the 1st failure wait 2s,
/// after the 2nd wait 8s. Long enough to outlast a busy upload reader
/// while staying interactive on the cold-start path.
const THUMB_RETRY_DELAYS: &[Duration] = &[Duration::from_secs(2), Duration::from_secs(8)];

/// Soft byte budget for CPU-side decoded media (decoded RGBA frames,
/// animated frame sequences, video/model posters, parsed model
/// geometry). Enforced once per frame by LRU eviction (issue #37).
/// Entries rendered last frame and entries backing an open lightbox are
/// exempt, so this is a soft cap on *off-screen* accumulation, not a
/// hard limit. ~512 MiB keeps a generous working set while bounding a
/// long session full of animated images.
const CPU_BUDGET_BYTES: usize = 512 * 1024 * 1024;

/// Soft byte budget for app-owned GPU textures (the `animated_gpu`
/// mirrors of animated previews). Same exemptions as
/// [`CPU_BUDGET_BYTES`]. Note that evicting an animated entry frees its
/// GPU mirror together with its CPU frames — the two budgets share one
/// LRU order.
const GPU_BUDGET_BYTES: usize = 256 * 1024 * 1024;

/// Nominal CPU cost charged for a parsed SVG entry. The real in-memory
/// size of a vector asset isn't observable from here, but it's small
/// compared to raster frames; a fixed estimate keeps pathological
/// many-SVG sessions bounded without overstating the common case.
const SVG_NOMINAL_BYTES: usize = 256 * 1024;

/// Which per-id "family" of cache maps an LRU entry covers. Eviction
/// always removes a family as a unit so linked state can't orphan
/// (e.g. evicting an animated image also drops its `gif_playback`
/// record and `animated_gpu` texture).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum MediaKind {
    /// `image_cache` entry + linked `gif_playback` + `animated_gpu`.
    Image,
    /// `lightbox_full` entry (full-res decode for an open lightbox).
    LightboxFull,
    /// `video_thumbs` poster.
    VideoThumb,
    /// `models` geometry + linked `model_thumbs` poster.
    Model,
}

/// Interior-mutable LRU bookkeeping. Lives behind a `Mutex` because the
/// lookup accessors that need to touch entries take `&self` (the render
/// path borrows the cache immutably). Contention is nil — everything
/// runs on the UI thread.
#[derive(Default)]
struct LruState {
    /// Monotonic use counter; bumped on every touch.
    clock: u64,
    /// Clock value at the previous budget-enforcement pass. Entries
    /// touched since then were used by the frame just built and are
    /// exempt from eviction (see [`plan_evictions`]).
    floor: u64,
    last_used: HashMap<(MediaKind, String), u64>,
    /// Transfer ids whose render lookup missed but for which we still
    /// hold a source path (previously decoded, since evicted). Drained
    /// once per frame to re-spawn the decode — the "evicted then
    /// re-scrolled" recovery path.
    demand: HashSet<String>,
}

impl LruState {
    /// Stamp an entry's family as used at the next clock tick.
    fn touch(&mut self, kind: MediaKind, id: &str) {
        self.clock += 1;
        let clock = self.clock;
        self.last_used.insert((kind, id.to_string()), clock);
    }
}

/// One evictable cache unit, as seen by [`plan_evictions`]: the whole
/// per-id family for its kind with its approximate byte costs.
struct EvictionCandidate {
    kind: MediaKind,
    id: String,
    last_used: u64,
    cpu_bytes: usize,
    gpu_bytes: usize,
}

/// Pure LRU eviction planner. Returns the `(kind, id)` families to
/// evict, least-recently-used first, until both totals fit their
/// budgets. Skips pinned ids (open lightboxes) and entries touched
/// after `recent_floor` (rendered last frame) — evicting those would
/// flicker visible content and trigger an immediate re-decode storm,
/// so the budgets are soft when the *visible* set alone exceeds them.
///
/// Pinning is deliberately id-scoped, not `(kind, id)`-scoped: an open
/// lightbox over an animated image renders through the *Image* family's
/// `animated_gpu` mirror even when `lightbox_full` has landed and the
/// chat row is scrolled away (so nothing touches the Image family). The
/// id-wide pin is what keeps that family alive — don't narrow it.
fn plan_evictions(
    mut candidates: Vec<EvictionCandidate>,
    cpu_budget: usize,
    gpu_budget: usize,
    recent_floor: u64,
    pinned: &HashSet<String>,
) -> Vec<(MediaKind, String)> {
    let mut cpu_total: usize = candidates.iter().map(|c| c.cpu_bytes).sum();
    let mut gpu_total: usize = candidates.iter().map(|c| c.gpu_bytes).sum();
    if cpu_total <= cpu_budget && gpu_total <= gpu_budget {
        return Vec::new();
    }
    candidates.sort_by_key(|c| c.last_used);
    let mut evict = Vec::new();
    for c in candidates {
        if cpu_total <= cpu_budget && gpu_total <= gpu_budget {
            break;
        }
        if pinned.contains(&c.id) || c.last_used > recent_floor {
            continue;
        }
        // Only evict entries that shrink a dimension that's actually
        // over budget — dropping a CPU-only poster does nothing for a
        // GPU overage.
        let helps_cpu = cpu_total > cpu_budget && c.cpu_bytes > 0;
        let helps_gpu = gpu_total > gpu_budget && c.gpu_bytes > 0;
        if !helps_cpu && !helps_gpu {
            continue;
        }
        cpu_total = cpu_total.saturating_sub(c.cpu_bytes);
        gpu_total = gpu_total.saturating_sub(c.gpu_bytes);
        evict.push((c.kind, c.id));
    }
    evict
}

/// Approximate CPU bytes of one decoded RGBA image.
fn image_bytes(img: &Image) -> usize {
    img.width() as usize * img.height() as usize * 4
}

/// Approximate CPU bytes of one [`chat::CachedImage`] entry.
fn cached_image_cpu_bytes(cached: &chat::CachedImage) -> usize {
    match cached {
        chat::CachedImage::Static(img) => image_bytes(img),
        chat::CachedImage::Animated { frames } => frames.iter().map(|(img, _)| image_bytes(img)).sum(),
        chat::CachedImage::Svg(_) => SVG_NOMINAL_BYTES,
    }
}

/// Approximate CPU bytes of parsed model geometry.
fn model_cpu_bytes(model: &LoadedModel) -> usize {
    use damascene_core::scene::{MeshVertex, ScenePoint};
    match model {
        LoadedModel::Mesh(handle) => {
            let (data, _) = handle.snapshot();
            data.vertices.len() * std::mem::size_of::<MeshVertex>()
                + data
                    .indices
                    .as_ref()
                    .map_or(0, |i| i.len() * std::mem::size_of::<u32>())
        }
        LoadedModel::Points(handle) => {
            let (data, _) = handle.snapshot();
            data.points.len() * std::mem::size_of::<ScenePoint>()
        }
    }
}

/// GPU bytes of one animated-preview texture (`Rgba8UnormSrgb`).
fn animated_gpu_bytes(gpu: &AnimatedGpu) -> usize {
    let (w, h) = gpu.size();
    w as usize * h as usize * 4
}

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

/// The chat history's row closures capture an `Arc<Mutex<MediaCache>>`
/// and must be `Send + Sync`, which requires `MediaCache: Send`. This
/// assertion turns a future non-`Send` field (e.g. an `Rc` sneaking
/// into a decoder) into an immediate, locatable compile error here
/// instead of an opaque one at the closure site.
const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<MediaCache>();
};

/// Centralized media cache. See module docs for the lifecycle model.
pub struct MediaCache {
    runtime: Handle,
    /// Pokes the host to schedule a frame. Called when an off-thread
    /// decode finishes so its result lands (via `drain_pending`) without
    /// waiting for the next input event — the app idles at 0fps, so a
    /// demand re-decode completing after the user stops scrolling would
    /// otherwise leave a placeholder on screen until the next event.
    repaint: Arc<dyn Fn() + Send + Sync>,

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
    /// In-flight off-thread image decodes, keyed by `transfer_id`.
    /// Guarded on both maps in `on_transfer_done` to match the model
    /// path's two-map discipline and prevent duplicate spawns.
    pending_image_decodes: HashMap<String, JoinHandle<Result<chat::CachedImage, image::ImageError>>>,

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

    /// LRU bookkeeping for the byte-budget eviction pass (issue #37).
    /// Interior-mutable so the `&self` lookup accessors can touch
    /// entries on use and record demand on miss.
    lru: Arc<Mutex<LruState>>,
    /// `transfer_id → (local_path, name)` for every media-typed
    /// transfer that has hit `Done`. Kept so an evicted entry can be
    /// re-decoded straight from disk when its message scrolls back
    /// into view — no new transfer event required. Tiny per entry
    /// (a path + name), so it isn't itself budgeted.
    sources: HashMap<String, (PathBuf, String)>,
    /// Transfer ids that must never be evicted: anything backing a
    /// currently-open (or opening) lightbox. Refreshed by the App once
    /// per frame, before [`Self::drain_pending`] runs the eviction pass.
    pinned: HashSet<String>,
}

impl MediaCache {
    pub fn new(runtime: Handle, gif_autoplay_default: bool, repaint: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self {
            runtime,
            repaint,
            gif_autoplay_default,
            image_cache: HashMap::new(),
            image_failed: HashSet::new(),
            pending_image_decodes: HashMap::new(),
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
            lru: Arc::new(Mutex::new(LruState::default())),
            sources: HashMap::new(),
            pinned: HashSet::new(),
        }
    }

    /// Spawn a blocking decode task that pokes the repaint callback on
    /// completion, so the result is drained into the cache on a prompt
    /// next frame instead of waiting for unrelated input.
    fn spawn_decode<T, F>(&self, f: F) -> JoinHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let repaint = self.repaint.clone();
        self.runtime.spawn_blocking(move || {
            let out = f();
            repaint();
            out
        })
    }

    /// Mark an entry as used at the current LRU clock. `&self` because
    /// the render path holds the cache immutably.
    fn touch(&self, kind: MediaKind, id: &str) {
        self.lru.lock().unwrap().touch(kind, id);
    }

    /// Record a render-time lookup miss for an id we previously decoded
    /// (its source path is still on file): next frame's
    /// [`Self::process_demand`] re-spawns the decode so evicted media
    /// repopulates when scrolled back into view.
    fn note_miss(&self, id: &str) {
        if !self.sources.contains_key(id) {
            return;
        }
        self.lru.lock().unwrap().demand.insert(id.to_string());
    }

    /// Replace the set of never-evict transfer ids. The App calls this
    /// once per frame (before [`Self::drain_pending`]) with the ids
    /// backing the open image/video/model lightboxes.
    pub fn set_pinned(&mut self, pinned: HashSet<String>) {
        self.pinned = pinned;
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

    /// Drive on a transfer that just landed at `local_path`. Image
    /// decoding (static and animated) runs off-thread via
    /// `spawn_blocking` — animated GIFs can be many MB and hundreds of
    /// frames, enough to freeze the UI for seconds on the UI thread.
    /// Video thumbs likewise go async; libmpv cold-start is 50–200ms.
    fn on_transfer_done(&mut self, id: &str, name: &str, local_path: &Path) {
        if chat::is_image_name(name) || crate::video::is_video_name(name) || model::is_model_name(name) {
            // Remember where the bytes live so an LRU-evicted entry can
            // be re-decoded from disk when it scrolls back into view.
            self.sources
                .insert(id.to_string(), (local_path.to_path_buf(), name.to_string()));
        }
        if chat::is_image_name(name)
            && !self.image_cache.contains_key(id)
            && !self.image_failed.contains(id)
            && !self.pending_image_decodes.contains_key(id)
        {
            let path = local_path.to_path_buf();
            let label = id.to_string();
            let handle = self.spawn_decode(move || decode_image(&path, Some(MAX_PREVIEW_PX), &label));
            self.pending_image_decodes.insert(id.to_string(), handle);
        }
        if crate::video::is_video_name(name)
            && !self.video_thumbs.contains_key(id)
            && !self.pending_video_thumbs.contains_key(id)
        {
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
            let handle = self.spawn_decode(move || model::load_model(&path));
            self.pending_model_loads.insert(id.to_string(), handle);
        }
    }

    /// Drive on a transfer that just terminated with `Failed`. Nothing
    /// to evict from the *successful* caches (a failed transfer's
    /// path was never decoded), but cancel any in-flight decode that
    /// somehow raced in. Belt-and-suspenders against future code
    /// paths that might fire Done→Failed in quick succession.
    fn on_transfer_failed(&mut self, id: &str) {
        if let Some(handle) = self.pending_image_decodes.remove(id) {
            handle.abort();
        }
        if let Some(handle) = self.pending_video_thumbs.remove(id) {
            handle.abort();
        }
        self.pending_video_thumb_retries.remove(id);
        if let Some(handle) = self.pending_model_loads.remove(id) {
            handle.abort();
        }
        // The local file is no longer trustworthy after a failure;
        // don't let the eviction-demand path re-decode from it.
        self.sources.remove(id);
    }

    fn spawn_video_thumb(&mut self, id: String, path: PathBuf) {
        let attempt = self.failed_video_thumbs.get(&id).map(|f| f.attempts + 1).unwrap_or(1);
        tracing::debug!(
            "media_cache: spawn video thumb extract for {id} attempt={attempt}/{THUMB_MAX_ATTEMPTS} path={}",
            path.display(),
        );
        let handle = self.spawn_decode(move || crate::video::extract_thumbnail(&path));
        self.pending_video_thumbs.insert(id.clone(), handle);
        if let Some(fail) = self.failed_video_thumbs.get_mut(&id) {
            fail.last_attempt = Instant::now();
        }
    }

    /// Land any off-thread decode results that finished since the
    /// last frame, plus respawn any video-thumb retries whose backoff
    /// has elapsed, then run the LRU byte-budget eviction pass.
    /// Called once per frame from `App::before_build` (after the App
    /// has refreshed [`Self::set_pinned`]).
    pub fn drain_pending(&mut self) {
        self.process_demand();
        self.drain_image_decodes();
        self.drain_video_thumbs();
        self.maybe_respawn_video_thumbs();
        self.drain_model_loads();
        self.drain_lightbox();
        self.enforce_budgets();
    }

    /// Re-spawn decodes for evicted entries whose render lookup missed
    /// last frame. Goes back through [`Self::on_transfer_done`], so all
    /// the in-flight / already-cached / known-failed guards apply and a
    /// repopulated entry behaves exactly like a fresh download.
    fn process_demand(&mut self) {
        let demanded: Vec<String> = {
            let mut lru = self.lru.lock().unwrap();
            if lru.demand.is_empty() {
                return;
            }
            lru.demand.drain().collect()
        };
        for id in demanded {
            let Some((path, name)) = self.sources.get(&id) else {
                continue;
            };
            // A video-thumb failure record means the retry/backoff
            // machinery (or its give-up decision) owns this id; the
            // demand path must not reset its counter every frame.
            if self.failed_video_thumbs.contains_key(&id) {
                continue;
            }
            let (path, name) = (path.clone(), name.clone());
            self.on_transfer_done(&id, &name, &path);
        }
    }

    /// LRU eviction down to the byte budgets. See [`plan_evictions`]
    /// for the policy (pinned + just-rendered entries are exempt).
    fn enforce_budgets(&mut self) {
        self.enforce_budgets_with(CPU_BUDGET_BYTES, GPU_BUDGET_BYTES);
    }

    fn enforce_budgets_with(&mut self, cpu_budget: usize, gpu_budget: usize) {
        let (recent_floor, last_used) = {
            let mut lru = self.lru.lock().unwrap();
            let floor = lru.floor;
            // Entries touched after this point are the next frame's
            // active set.
            lru.floor = lru.clock;
            (floor, lru.last_used.clone())
        };
        let stamp = |kind: MediaKind, id: &str| last_used.get(&(kind, id.to_string())).copied().unwrap_or(0);

        let mut candidates: Vec<EvictionCandidate> = Vec::new();
        for (id, cached) in &self.image_cache {
            candidates.push(EvictionCandidate {
                kind: MediaKind::Image,
                id: id.clone(),
                last_used: stamp(MediaKind::Image, id),
                cpu_bytes: cached_image_cpu_bytes(cached),
                gpu_bytes: self.animated_gpu.get(id).map_or(0, animated_gpu_bytes),
            });
        }
        for (id, cached) in &self.lightbox_full {
            candidates.push(EvictionCandidate {
                kind: MediaKind::LightboxFull,
                id: id.clone(),
                last_used: stamp(MediaKind::LightboxFull, id),
                cpu_bytes: cached_image_cpu_bytes(cached),
                gpu_bytes: 0,
            });
        }
        for (id, thumb) in &self.video_thumbs {
            candidates.push(EvictionCandidate {
                kind: MediaKind::VideoThumb,
                id: id.clone(),
                last_used: stamp(MediaKind::VideoThumb, id),
                cpu_bytes: image_bytes(thumb),
                gpu_bytes: 0,
            });
        }
        for (id, model) in &self.models {
            candidates.push(EvictionCandidate {
                kind: MediaKind::Model,
                id: id.clone(),
                last_used: stamp(MediaKind::Model, id),
                cpu_bytes: model_cpu_bytes(model) + self.model_thumbs.get(id).map_or(0, image_bytes),
                gpu_bytes: 0,
            });
        }

        for (kind, id) in plan_evictions(candidates, cpu_budget, gpu_budget, recent_floor, &self.pinned) {
            self.evict_entry(kind, &id);
        }
    }

    /// Remove one per-id family from the cache, including all linked
    /// state — evicting an animated image drops its `gif_playback`
    /// record and `animated_gpu` texture in the same step so neither
    /// can orphan. Deliberately does *not* mark the id as failed: a
    /// later render miss re-decodes it via [`Self::process_demand`].
    fn evict_entry(&mut self, kind: MediaKind, id: &str) {
        self.lru.lock().unwrap().last_used.remove(&(kind, id.to_string()));
        match kind {
            MediaKind::Image => {
                self.image_cache.remove(id);
                self.gif_playback.remove(id);
                self.animated_gpu.remove(id);
            }
            MediaKind::LightboxFull => {
                self.lightbox_full.remove(id);
            }
            MediaKind::VideoThumb => {
                self.video_thumbs.remove(id);
            }
            MediaKind::Model => {
                self.models.remove(id);
                self.model_thumbs.remove(id);
            }
        }
        tracing::debug!("media_cache: LRU-evicted {kind:?} entry for {id} (over byte budget)");
    }

    /// Land finished off-thread image decodes. Animated results get a
    /// `GifPlayback` entry using the current autoplay default; static
    /// and SVG results go straight into `image_cache`. Failures are
    /// recorded in `image_failed` so subsequent Done events for the
    /// same id are no-ops.
    fn drain_image_decodes(&mut self) {
        let finished: Vec<String> = self
            .pending_image_decodes
            .iter()
            .filter(|(_, h)| h.is_finished())
            .map(|(id, _)| id.clone())
            .collect();
        for id in finished {
            let handle = self.pending_image_decodes.remove(&id).unwrap();
            match self.runtime.block_on(handle) {
                Ok(Ok(cached)) => {
                    if cached.is_animated() {
                        self.gif_playback
                            .entry(id.clone())
                            .or_insert_with(|| chat::GifPlayback::new(self.gif_autoplay_default));
                    }
                    self.touch(MediaKind::Image, &id);
                    self.image_cache.insert(id, cached);
                }
                Ok(Err(e)) => {
                    tracing::debug!("image preview decode failed for {id}: {e}");
                    self.image_failed.insert(id);
                }
                Err(join_err) => {
                    tracing::warn!("image decode task panicked for {id}: {join_err}");
                    self.image_failed.insert(id);
                }
            }
        }
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
                    self.touch(MediaKind::Model, &id);
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
                    self.touch(MediaKind::VideoThumb, &id);
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
                self.touch(MediaKind::LightboxFull, &pending.transfer_id);
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
                self.touch(MediaKind::Model, &id);
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
        let label = transfer_id.to_string();
        let handle = self.spawn_decode(move || decode_image(&path, Some(MAX_LIGHTBOX_PX), &label));
        self.pending_lightbox = Some(PendingLightboxDecode {
            transfer_id: transfer_id.to_string(),
            handle,
        });
    }

    /// Drop the active lightbox decode + any cached full-res image.
    /// Frees GPU memory and prevents a late-landing decode from
    /// re-populating the cache after the user closed the overlay.
    pub fn close_lightbox(&mut self) {
        // Drop the LRU stamps with the entries so close/reopen cycles
        // don't accumulate stale `(LightboxFull, id)` records.
        let mut lru = self.lru.lock().unwrap();
        for id in self.lightbox_full.keys() {
            lru.last_used.remove(&(MediaKind::LightboxFull, id.clone()));
        }
        drop(lru);
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
    //
    // Every hit touches the entry's LRU stamp (the render path is what
    // keeps on-screen media alive); a miss on a previously-decoded id
    // records re-decode demand (see [`Self::note_miss`]).

    pub fn image_for(&self, transfer_id: &str) -> Option<&chat::CachedImage> {
        match self.image_cache.get(transfer_id) {
            Some(cached) => {
                self.touch(MediaKind::Image, transfer_id);
                Some(cached)
            }
            None => {
                self.note_miss(transfer_id);
                None
            }
        }
    }

    pub fn gif_playback_for(&self, transfer_id: &str) -> Option<&chat::GifPlayback> {
        self.gif_playback.get(transfer_id)
    }

    pub fn animated_gpu_for(&self, transfer_id: &str) -> Option<&AnimatedGpu> {
        self.animated_gpu.get(transfer_id)
    }

    pub fn video_thumb_for(&self, transfer_id: &str) -> Option<&Image> {
        match self.video_thumbs.get(transfer_id) {
            Some(thumb) => {
                self.touch(MediaKind::VideoThumb, transfer_id);
                Some(thumb)
            }
            None => {
                self.note_miss(transfer_id);
                None
            }
        }
    }

    /// Poster thumbnail for a downloaded 3D model, if one has rendered.
    pub fn model_thumb_for(&self, transfer_id: &str) -> Option<&Image> {
        match self.model_thumbs.get(transfer_id) {
            Some(thumb) => {
                self.touch(MediaKind::Model, transfer_id);
                Some(thumb)
            }
            None => {
                self.note_miss(transfer_id);
                None
            }
        }
    }

    /// Parsed geometry for a downloaded 3D model, if the parse succeeded.
    /// The lightbox clones this (cheap `Arc` bump) to build its scene.
    pub fn model_for(&self, transfer_id: &str) -> Option<&LoadedModel> {
        match self.models.get(transfer_id) {
            Some(model) => {
                self.touch(MediaKind::Model, transfer_id);
                Some(model)
            }
            None => {
                self.note_miss(transfer_id);
                None
            }
        }
    }

    /// Best image available for the open lightbox: full-resolution if
    /// the decode has landed, otherwise the inline thumbnail.
    pub fn lightbox_image_for(&self, transfer_id: &str) -> Option<&chat::CachedImage> {
        if let Some(full) = self.lightbox_full.get(transfer_id) {
            self.touch(MediaKind::LightboxFull, transfer_id);
            return Some(full);
        }
        self.image_for(transfer_id)
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
/// `None` loads the source at its natural resolution. `label` is a
/// transfer-id string used only in diagnostic log messages.
///
/// Animated GIF, animated WebP, and APNG decode every frame and
/// produce [`chat::CachedImage::Animated`]; single-frame animations
/// and all other formats collapse to `Static`. SVG goes through
/// damascene's vector parser.
fn decode_image(path: &Path, max_px: Option<u32>, label: &str) -> Result<chat::CachedImage, image::ImageError> {
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
            return collect_animated_frames(decoder.into_frames(), max_px, label);
        }
        Some(ImageFormat::WebP) => {
            let decoder = WebPDecoder::new(BufReader::new(std::fs::File::open(path)?))?;
            if decoder.has_animation() {
                return collect_animated_frames(decoder.into_frames(), max_px, label);
            }
        }
        Some(ImageFormat::Png) => {
            let decoder = PngDecoder::new(BufReader::new(std::fs::File::open(path)?))?;
            if decoder.is_apng()? {
                return collect_animated_frames(decoder.apng()?.into_frames(), max_px, label);
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
///
/// Truncates to at most [`MAX_ANIMATED_FRAMES`] frames or
/// [`MAX_ANIMATED_RGBA_BYTES`] total pre-thumbnail RGBA bytes,
/// whichever limit is hit first. Truncated animations loop over the
/// frames collected so far. `label` identifies the transfer in
/// diagnostic messages.
fn collect_animated_frames(
    frames: image::Frames<'_>,
    max_px: Option<u32>,
    label: &str,
) -> Result<chat::CachedImage, image::ImageError> {
    use image::DynamicImage;

    let mut out: Vec<(Image, Duration)> = Vec::new();
    let mut total_decoded_bytes: usize = 0;
    let mut truncated = false;

    for frame in frames {
        // Check frame-count cap before decoding the next frame.
        if out.len() >= MAX_ANIMATED_FRAMES {
            truncated = true;
            break;
        }

        let frame = frame?;
        let (numer, denom) = frame.delay().numer_denom_ms();
        let micros = (numer as u64).saturating_mul(1000) / (denom.max(1) as u64);
        let delay = Duration::from_micros(micros).max(Duration::from_millis(20));

        let mut dynimg = DynamicImage::ImageRgba8(frame.into_buffer());

        // Byte budget checked at pre-thumbnail resolution; that is the
        // peak allocation point during decode.
        let frame_bytes = dynimg.width() as usize * dynimg.height() as usize * 4;
        if total_decoded_bytes.saturating_add(frame_bytes) > MAX_ANIMATED_RGBA_BYTES {
            truncated = true;
            break;
        }
        total_decoded_bytes += frame_bytes;

        if let Some(cap) = max_px
            && dynimg.width().max(dynimg.height()) > cap
        {
            dynimg = dynimg.thumbnail(cap, cap);
        }
        let rgba = dynimg.to_rgba8();
        let img = Image::from_rgba8(rgba.width(), rgba.height(), rgba.into_raw());
        out.push((img, delay));
    }

    if truncated {
        tracing::warn!(
            "media_cache: animated image {label}: truncated to {} frames ({:.1} MiB pre-thumbnail) — exceeded cap \
             ({MAX_ANIMATED_FRAMES} frames / {} MiB)",
            out.len(),
            total_decoded_bytes as f64 / (1024.0 * 1024.0),
            MAX_ANIMATED_RGBA_BYTES / (1024 * 1024),
        );
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(kind: MediaKind, id: &str, last_used: u64, cpu: usize, gpu: usize) -> EvictionCandidate {
        EvictionCandidate {
            kind,
            id: id.to_string(),
            last_used,
            cpu_bytes: cpu,
            gpu_bytes: gpu,
        }
    }

    fn ids(plan: &[(MediaKind, String)]) -> Vec<&str> {
        plan.iter().map(|(_, id)| id.as_str()).collect()
    }

    #[test]
    fn under_budget_evicts_nothing() {
        let plan = plan_evictions(
            vec![
                cand(MediaKind::Image, "a", 1, 100, 0),
                cand(MediaKind::VideoThumb, "b", 2, 100, 0),
            ],
            1000,
            1000,
            u64::MAX,
            &HashSet::new(),
        );
        assert!(plan.is_empty());
    }

    #[test]
    fn evicts_least_recently_used_first_until_under_budget() {
        let plan = plan_evictions(
            vec![
                cand(MediaKind::Image, "newest", 30, 100, 0),
                cand(MediaKind::Image, "oldest", 10, 100, 0),
                cand(MediaKind::Image, "middle", 20, 100, 0),
            ],
            150,
            1000,
            u64::MAX,
            &HashSet::new(),
        );
        // Total 300 over a 150 budget: dropping oldest (→200) isn't
        // enough, dropping middle too (→100) is; newest survives.
        assert_eq!(ids(&plan), vec!["oldest", "middle"]);
    }

    #[test]
    fn pinned_entries_survive_even_over_budget() {
        let pinned: HashSet<String> = ["oldest".to_string()].into_iter().collect();
        let plan = plan_evictions(
            vec![
                cand(MediaKind::Image, "oldest", 10, 100, 0),
                cand(MediaKind::Image, "newer", 20, 100, 0),
            ],
            0,
            0,
            u64::MAX,
            &pinned,
        );
        assert_eq!(ids(&plan), vec!["newer"]);
    }

    #[test]
    fn recently_used_entries_survive() {
        // floor = 15: "fresh" (20 > 15) was rendered last frame and is
        // exempt; "stale" (10 ≤ 15) is fair game.
        let plan = plan_evictions(
            vec![
                cand(MediaKind::Image, "stale", 10, 100, 0),
                cand(MediaKind::Image, "fresh", 20, 100, 0),
            ],
            0,
            0,
            15,
            &HashSet::new(),
        );
        assert_eq!(ids(&plan), vec!["stale"]);
    }

    #[test]
    fn gpu_only_overage_skips_cpu_only_entries() {
        let plan = plan_evictions(
            vec![
                cand(MediaKind::VideoThumb, "cpu_only", 1, 100, 0),
                cand(MediaKind::Image, "animated", 2, 100, 500),
            ],
            1000,
            300,
            u64::MAX,
            &HashSet::new(),
        );
        // Only the GPU budget is exceeded — evicting the CPU-only
        // poster would be pure waste even though it's older.
        assert_eq!(ids(&plan), vec!["animated"]);
    }

    #[test]
    fn cached_image_cost_counts_all_animated_frames() {
        let frame = || (Image::from_rgba8(2, 2, vec![0; 16]), Duration::from_millis(20));
        let animated = chat::CachedImage::Animated {
            frames: vec![frame(), frame(), frame()],
        };
        assert_eq!(cached_image_cpu_bytes(&animated), 3 * 2 * 2 * 4);
        let single = chat::CachedImage::Static(Image::from_rgba8(4, 2, vec![0; 32]));
        assert_eq!(cached_image_cpu_bytes(&single), 4 * 2 * 4);
    }

    #[test]
    fn evicting_animated_image_cleans_playback_and_lru_meta() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut mc = MediaCache::new(rt.handle().clone(), true, Arc::new(|| {}));
        let frame = || (Image::from_rgba8(2, 2, vec![0; 16]), Duration::from_millis(20));
        mc.image_cache.insert(
            "anim".into(),
            chat::CachedImage::Animated {
                frames: vec![frame(), frame()],
            },
        );
        mc.gif_playback.insert("anim".into(), chat::GifPlayback::new(true));
        mc.touch(MediaKind::Image, "anim");

        // First pass: the entry was touched since the last floor, so it
        // counts as "in active use" and survives even at zero budget.
        mc.enforce_budgets_with(0, 0);
        assert!(mc.image_cache.contains_key("anim"));

        // Second pass: no touch since the previous pass → evicted, and
        // the linked playback record goes with it (the GPU mirror would
        // too — same `evict_entry` arm — but needs a wgpu device to
        // construct in a test).
        mc.enforce_budgets_with(0, 0);
        assert!(mc.image_cache.is_empty());
        assert!(mc.gif_playback.is_empty());
        assert!(mc.lru.lock().unwrap().last_used.is_empty());
        // Not marked failed: a re-decode stays possible.
        assert!(!mc.image_failed.contains("anim"));
    }

    #[test]
    fn decode_completion_pokes_repaint() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let rt = tokio::runtime::Runtime::new().unwrap();
        let pokes = Arc::new(AtomicUsize::new(0));
        let pokes_in_cb = pokes.clone();
        let mut mc = MediaCache::new(
            rt.handle().clone(),
            true,
            Arc::new(move || {
                pokes_in_cb.fetch_add(1, Ordering::SeqCst);
            }),
        );

        let dir = std::env::temp_dir().join(format!("rumble-media-repaint-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("img.png");
        image::RgbaImage::from_pixel(4, 4, image::Rgba([10, 20, 30, 255]))
            .save(&path)
            .unwrap();

        mc.on_transfer_done("t1", "img.png", &path);
        for _ in 0..500 {
            mc.drain_pending();
            if mc.image_cache.contains_key("t1") {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(mc.image_cache.contains_key("t1"));
        assert!(pokes.load(Ordering::SeqCst) >= 1, "decode completion must poke repaint");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pinned_id_survives_enforcement_on_the_cache() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut mc = MediaCache::new(rt.handle().clone(), true, Arc::new(|| {}));
        mc.image_cache.insert(
            "pinned".into(),
            chat::CachedImage::Static(Image::from_rgba8(2, 2, vec![0; 16])),
        );
        mc.set_pinned(["pinned".to_string()].into_iter().collect());
        mc.enforce_budgets_with(0, 0);
        mc.enforce_budgets_with(0, 0);
        assert!(mc.image_cache.contains_key("pinned"));
    }

    #[test]
    fn evicted_image_redecodes_on_render_demand() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut mc = MediaCache::new(rt.handle().clone(), true, Arc::new(|| {}));

        let dir = std::env::temp_dir().join(format!("rumble-media-cache-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("img.png");
        image::RgbaImage::from_pixel(4, 4, image::Rgba([10, 20, 30, 255]))
            .save(&path)
            .unwrap();

        let wait_for_decode = |mc: &mut MediaCache| {
            for _ in 0..500 {
                mc.drain_pending();
                if mc.image_cache.contains_key("t1") {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("decode never landed");
        };

        mc.on_transfer_done("t1", "img.png", &path);
        wait_for_decode(&mut mc);

        // Evict (two passes so the insert-time touch ages out).
        mc.enforce_budgets_with(0, 0);
        mc.enforce_budgets_with(0, 0);
        assert!(mc.image_cache.is_empty());
        assert!(!mc.image_failed.contains("t1"));

        // A render-time miss records demand…
        assert!(mc.image_for("t1").is_none());
        // …and the next frames re-decode from the still-on-disk file.
        wait_for_decode(&mut mc);
        assert!(mc.image_for("t1").is_some());

        std::fs::remove_dir_all(&dir).ok();
    }
}
