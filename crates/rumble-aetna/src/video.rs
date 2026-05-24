//! Video playback in chat: extension detection, the active-video
//! state the App holds while a video lightbox is open, and the
//! lightbox renderer.
//!
//! Phase-3 scope: lightbox-only playback. A click on a downloaded
//! video file's "Play" button spawns a `VideoStream` via libmpv,
//! routes its frames through a per-stream `VideoGpu` mirror, and
//! composites them through aetna's `surface()` widget into a
//! lightbox panel with play/pause + scrub bar + time + mute. No
//! inline previews or autoplay yet — those are Phase 3b.
//!
//! Audio is left to libmpv's default output (`ao=auto` in
//! `rumble-video`'s `MpvPlayer`). Volume control is handled at the
//! libmpv layer too via the `volume` / `mute` properties.

use std::{path::Path, sync::LazyLock, time::Duration};

use aetna_core::{prelude::*, surface::SurfaceAlpha};
use rumble_video::{Error as VideoError, MpvPlayer, VideoGpu, VideoStream};

// ---- Routing keys ----

pub const KEY_LIGHTBOX_DISMISS: &str = "video:lightbox:dismiss";
pub const KEY_LIGHTBOX_CLOSE: &str = "video:lightbox:close";
pub const KEY_LIGHTBOX_SURFACE: &str = "video:lightbox:surface";
pub const KEY_PLAY_PAUSE: &str = "video:lightbox:playpause";
pub const KEY_MUTE: &str = "video:lightbox:mute";
pub const KEY_SCRUB: &str = "video:lightbox:scrub";

/// Routed key for "open this transfer in the video lightbox", used
/// on the play button rendered inside [`crate::chat::file_offer_card`]
/// when the file is a downloaded video.
pub fn open_video_key(transfer_id: &str) -> String {
    format!("video:open:{transfer_id}")
}

pub fn parse_open_video_key(key: &str) -> Option<&str> {
    key.strip_prefix("video:open:")
}

// ---- Format detection ----

/// True when `name`'s extension is one libmpv can plausibly play.
/// Whitelist instead of blacklist — opening an unsupported file
/// would block for the libmpv connect-and-fail path before
/// surfacing as a `LoadFailed`. Mirrors what most chat clients
/// allow as inline-playable video.
pub fn is_video_name(name: &str) -> bool {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    matches!(ext.as_str(), "mp4" | "m4v" | "mov" | "webm" | "mkv" | "avi" | "ogv")
}

// ---- Thumbnail extraction ----

/// Cap on the longest edge of an extracted poster thumbnail. The
/// chat preview only needs to look good at ~220px tall plus a 2x
/// HiDPI factor, so 1024 is plenty and keeps memory pressure
/// bounded for 4K source video.
const POSTER_MAX_DIM: u32 = 1024;

/// Decode the first frame of `path` into an aetna [`Image`] sized
/// for use as an inline-preview poster. Runs synchronously — the
/// caller is expected to drive this from `runtime.spawn_blocking`
/// so the 50–200ms libmpv connect/decode hit doesn't stall the
/// UI thread.
///
/// Internally: spin up a one-shot [`MpvPlayer`] (audio off, hwdec
/// off, paused), `load_file`, ask libmpv to scale the first
/// frame into a buffer sized to [`POSTER_MAX_DIM`]'s longest
/// edge, then wrap the RGBA bytes in an `Image`.
pub fn extract_thumbnail(path: &Path) -> Result<Image, VideoError> {
    let player = MpvPlayer::new()?;
    // Headless / fast-path config: no audio device, no hardware
    // decode (slower decode but no GPU contention with the host
    // wgpu device), paused so the decoder doesn't race ahead.
    player.set_option_string("audio", "no")?;
    player.set_option_string("hwdec", "no")?;
    player.set_option_string("pause", "yes")?;

    player.load_file(path)?;
    let (nat_w, nat_h) = player.dimensions()?;

    // Downsample at render time — libmpv handles the scaling for
    // us via the SW render API.
    let (w, h) = scale_to_fit(nat_w, nat_h, POSTER_MAX_DIM);
    let stride = (w as usize) * 4;
    let mut buf = vec![0u8; stride * h as usize];

    // First frame typically lands within a few hundred ms. The
    // ceiling has to cover libmpv's cold-start: on the very first
    // extraction after process launch, codec init can take a few
    // seconds on its own before the file even opens. 15s gives
    // that headroom without inviting a truly stuck decoder to
    // block forever — the pump retries on failure now, so a
    // tighter cap doesn't buy us much.
    if !player.wait_for_frame(Duration::from_secs(15))? {
        return Err(VideoError::Timeout("first frame for thumbnail"));
    }
    player.render_sw(&mut buf, w, h, stride)?;

    Ok(Image::from_rgba8(w, h, buf))
}

/// Fit `(w, h)` into a box of `max` on its longest edge,
/// preserving aspect ratio. Result is always at least 1×1.
fn scale_to_fit(w: u32, h: u32, max: u32) -> (u32, u32) {
    if w == 0 || h == 0 {
        return (1, 1);
    }
    let longest = w.max(h);
    if longest <= max {
        return (w, h);
    }
    let scale = max as f64 / longest as f64;
    let new_w = ((w as f64 * scale).round() as u32).max(1);
    let new_h = ((h as f64 * scale).round() as u32).max(1);
    (new_w, new_h)
}

// ---- Active video state ----

/// State for the currently-open video lightbox. Owns the libmpv
/// stream + decode worker (`stream`) and the GPU mirror (`gpu`,
/// allocated lazily once the host's wgpu device is available).
/// Drop tears down the worker thread, libmpv handle, and GPU
/// texture in the right order.
pub struct ActiveVideo {
    pub transfer_id: String,
    pub name: String,
    pub stream: VideoStream,
    /// `None` between `ActiveVideo::new` and the next
    /// `before_paint` — allocation needs the wgpu device, which
    /// the host provides separately.
    pub gpu: Option<VideoGpu>,
    /// Mirrored from the libmpv `pause` property so the UI can
    /// flip the play/pause icon synchronously without round-
    /// tripping through a property observer.
    pub playing: bool,
    /// Mirrored from libmpv's `mute` property.
    pub muted: bool,
    /// `true` while the user is actively dragging the scrub bar.
    /// We seek on every drag tick — libmpv handles it fine — but
    /// suppress the time-pos auto-update so the thumb doesn't
    /// jitter back to the playback head between frames.
    pub scrubbing: bool,
    /// Most recent normalized scrub position. Reflects the user's
    /// dragged target while `scrubbing == true`; otherwise echoes
    /// `time_pos / duration` for paint.
    pub scrub_value: f32,
}

impl ActiveVideo {
    /// Wrap a freshly-opened stream into an `ActiveVideo`.
    /// Defaults: playing, unmuted. The actual libmpv state matches
    /// (the player starts unpaused with default volume).
    pub fn new(transfer_id: impl Into<String>, name: impl Into<String>, stream: VideoStream) -> Self {
        Self {
            transfer_id: transfer_id.into(),
            name: name.into(),
            stream,
            gpu: None,
            playing: true,
            muted: false,
            scrubbing: false,
            scrub_value: 0.0,
        }
    }

    /// Flip play/pause both UI-side and libmpv-side. Errors from
    /// libmpv are logged but not surfaced — pause is best-effort
    /// UX, no point bubbling failures into the render loop.
    pub fn toggle_play(&mut self) {
        let next = !self.playing;
        let res = if next {
            self.stream.resume()
        } else {
            self.stream.pause()
        };
        match res {
            Ok(()) => self.playing = next,
            Err(e) => tracing::warn!(error = %e, "video: toggle_play failed"),
        }
    }

    /// Flip mute both UI-side and libmpv-side via the `mute`
    /// property. `volume` itself is left at the libmpv default
    /// (100); a later phase can add a slider if users ask for it.
    pub fn toggle_mute(&mut self) {
        let next = !self.muted;
        let value = if next { "yes" } else { "no" };
        match self.stream.set_property_string("mute", value) {
            Ok(()) => self.muted = next,
            Err(e) => tracing::warn!(error = %e, "video: toggle_mute failed"),
        }
    }

    /// Seek to a fraction `[0, 1]` of the file's duration. No-op
    /// when libmpv hasn't reported a duration yet.
    pub fn seek_normalized(&mut self, n: f32) {
        let n = n.clamp(0.0, 1.0);
        self.scrub_value = n;
        let dur = match self.stream.duration() {
            Ok(Some(d)) => d,
            _ => return,
        };
        let target = Duration::from_secs_f64(n as f64 * dur.as_secs_f64());
        if let Err(e) = self.stream.seek(target) {
            tracing::warn!(error = %e, "video: seek failed");
        }
    }

    /// Seek `delta_secs` relative to the current playhead.
    /// Negative steps backward, positive forward; clamps the
    /// target into `[0, duration]`. Used by the keyboard
    /// shortcuts for arrow-key seek. No-op when time-pos /
    /// duration aren't available yet.
    pub fn seek_relative(&mut self, delta_secs: f64) {
        let pos = match self.stream.time_pos() {
            Ok(Some(d)) => d.as_secs_f64(),
            _ => return,
        };
        let dur = match self.stream.duration() {
            Ok(Some(d)) => d.as_secs_f64(),
            _ => return,
        };
        let target = (pos + delta_secs).clamp(0.0, dur);
        if let Err(e) = self.stream.seek(Duration::from_secs_f64(target)) {
            tracing::warn!(error = %e, "video: seek_relative failed");
        }
        // Reflect the new position in the scrub bar immediately
        // so the thumb doesn't visibly trail by one
        // refresh_scrub_value cycle. The next frame's refresh
        // will overwrite this from time-pos anyway, but the
        // initial paint after the keypress feels much snappier.
        if dur > f64::EPSILON {
            self.scrub_value = (target / dur) as f32;
        }
    }

    /// Seek to start (0) or end (just before duration) of the
    /// stream. The "just before" matters for non-looped streams:
    /// landing exactly on duration tends to immediately fire
    /// END_FILE rather than displaying the last frame.
    pub fn seek_to_edge(&mut self, end: bool) {
        if !end {
            self.seek_normalized(0.0);
            return;
        }
        let dur = match self.stream.duration() {
            Ok(Some(d)) => d.as_secs_f64(),
            _ => return,
        };
        // Park 250ms shy of the very end so loop-file=inf has a
        // moment to wrap around without snapping to the first
        // frame mid-keypress.
        let target = (dur - 0.25).max(0.0);
        if let Err(e) = self.stream.seek(Duration::from_secs_f64(target)) {
            tracing::warn!(error = %e, "video: seek_to_edge failed");
        }
        if dur > f64::EPSILON {
            self.scrub_value = (target / dur) as f32;
        }
    }

    /// Refresh the scrub-bar display from the live playback head
    /// when the user isn't actively dragging. Called once per
    /// frame from `App::before_build`.
    pub fn refresh_scrub_value(&mut self) {
        if self.scrubbing {
            return;
        }
        let pos = self.stream.time_pos().ok().flatten();
        let dur = self.stream.duration().ok().flatten();
        if let (Some(pos), Some(dur)) = (pos, dur)
            && dur.as_secs_f64() > f64::EPSILON
        {
            self.scrub_value = (pos.as_secs_f64() / dur.as_secs_f64()) as f32;
            self.scrub_value = self.scrub_value.clamp(0.0, 1.0);
        }
    }
}

// ---- Keyboard shortcuts ----

/// Seek step, in seconds, applied by Left/Right without modifiers.
/// Tuned to match common video-player muscle memory (mpv defaults
/// to 5s, VLC to 10s; we land in between).
const SEEK_STEP_SMALL: f64 = 5.0;
/// Coarser seek step for Shift+Left / Shift+Right. mpv uses 60s
/// here; 30s feels less jarring for short chat clips.
const SEEK_STEP_LARGE: f64 = 30.0;

/// Try to handle `event` as a video-lightbox keyboard shortcut.
/// Returns `true` if the event was consumed (the caller should
/// `return` from `on_event` without further processing). Only
/// inspects [`UiEventKind::KeyDown`] events — other event kinds
/// are ignored (and pass through unchanged).
pub fn handle_lightbox_key(active: &mut ActiveVideo, event: &UiEvent) -> bool {
    if event.kind != UiEventKind::KeyDown {
        return false;
    }
    let Some(kp) = event.key_press.as_ref() else {
        return false;
    };
    let mods = kp.modifiers;
    // Block all shortcuts when Ctrl/Alt/Logo are held — those
    // belong to the OS / app-level chord namespace, not to the
    // lightbox.
    if mods.ctrl || mods.alt || mods.logo {
        return false;
    }

    match &kp.key {
        UiKey::Space if !mods.shift => {
            active.toggle_play();
            true
        }
        UiKey::ArrowLeft => {
            let step = if mods.shift { SEEK_STEP_LARGE } else { SEEK_STEP_SMALL };
            active.seek_relative(-step);
            true
        }
        UiKey::ArrowRight => {
            let step = if mods.shift { SEEK_STEP_LARGE } else { SEEK_STEP_SMALL };
            active.seek_relative(step);
            true
        }
        UiKey::Home if !mods.shift => {
            active.seek_to_edge(false);
            true
        }
        UiKey::End if !mods.shift => {
            active.seek_to_edge(true);
            true
        }
        UiKey::Character(s) if s.eq_ignore_ascii_case("m") && !mods.shift => {
            active.toggle_mute();
            true
        }
        _ => false,
    }
}

// ---- Lightbox renderer ----

static SVG_PLAY: LazyLock<SvgIcon> =
    LazyLock::new(|| SvgIcon::parse_current_color(include_str!("../assets/icons/play.svg")).expect("play.svg parses"));
static SVG_PAUSE: LazyLock<SvgIcon> = LazyLock::new(|| {
    SvgIcon::parse_current_color(include_str!("../assets/icons/pause.svg")).expect("pause.svg parses")
});
static SVG_VOLUME_ON: LazyLock<SvgIcon> = LazyLock::new(|| {
    SvgIcon::parse_current_color(include_str!("../assets/icons/volume_on.svg")).expect("volume_on.svg parses")
});
static SVG_VOLUME_OFF: LazyLock<SvgIcon> = LazyLock::new(|| {
    SvgIcon::parse_current_color(include_str!("../assets/icons/volume_off.svg")).expect("volume_off.svg parses")
});

/// Format `d` as `m:ss` or `h:mm:ss`. Used for the time display
/// next to the scrub bar.
fn format_time(d: Duration) -> String {
    let total = d.as_secs();
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Build the click-to-enlarge video viewer overlay. Mirrors
/// `chat::render_lightbox`'s shape: scrim + centered panel +
/// header + body + controls.
pub fn render_lightbox(active: &ActiveVideo) -> El {
    let header = lightbox_header(active);
    let body = lightbox_body(active);
    let controls = lightbox_controls(active);

    let panel = column([header, body, controls])
        .style_profile(StyleProfile::Surface)
        .surface_role(SurfaceRole::Popover)
        .fill(tokens::POPOVER)
        .stroke(tokens::BORDER)
        .stroke_width(1.0)
        .radius(tokens::RADIUS_LG)
        .padding(Sides::all(tokens::SPACE_4))
        .gap(tokens::SPACE_3)
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0))
        .block_pointer();

    let centered = stack([panel]).fill_size().padding(Sides::all(tokens::SPACE_7));

    overlay([scrim(KEY_LIGHTBOX_DISMISS), centered])
}

fn lightbox_header(active: &ActiveVideo) -> El {
    row([
        text(active.name.clone()).semibold().ellipsis(),
        spacer().width(Size::Fill(1.0)),
        button("Close").key(KEY_LIGHTBOX_CLOSE).ghost(),
    ])
    .gap(tokens::SPACE_2)
    .align(Align::Center)
    .width(Size::Fill(1.0))
}

fn lightbox_body(active: &ActiveVideo) -> El {
    // surface() into the panel body with `Contain` letterboxing.
    // The 33ms `redraw_within` keeps aetna's loop ticking while
    // playback advances; off-screen lightboxes (panel hidden /
    // resized away) drop out of the redraw aggregator
    // automatically.
    let inner: El = match active.gpu.as_ref() {
        Some(gpu) => surface(gpu.app_texture().clone())
            .surface_alpha(SurfaceAlpha::Opaque)
            .surface_fit(ImageFit::Contain)
            .radius(tokens::RADIUS_MD)
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0))
            .redraw_within(Duration::from_millis(33)),
        None => text("Loading…").muted().width(Size::Fill(1.0)).height(Size::Fill(1.0)),
    };

    // Focusable surface so keyboard events route here once the
    // user clicks (or Tabs) to it. Click/Activate flips
    // play/pause — matches YouTube / most native players. Once
    // the surface is the focus target, Space → Activate fires
    // here too (aetna translates Space-with-focus to Activate
    // before our KeyDown handler ever sees it).
    stack([inner])
        .key(KEY_LIGHTBOX_SURFACE)
        .focusable()
        .cursor(Cursor::Pointer)
        .clip()
        .fill(tokens::CARD)
        .radius(tokens::RADIUS_MD)
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0))
}

fn lightbox_controls(active: &ActiveVideo) -> El {
    let play_icon: IconSource = if active.playing {
        SVG_PAUSE.clone().into()
    } else {
        SVG_PLAY.clone().into()
    };
    let play_btn = icon_button(play_icon)
        .key(KEY_PLAY_PAUSE)
        .ghost()
        .tooltip(if active.playing { "Pause" } else { "Play" });

    let mute_icon: IconSource = if active.muted {
        SVG_VOLUME_OFF.clone().into()
    } else {
        SVG_VOLUME_ON.clone().into()
    };
    let mute_btn = icon_button(mute_icon)
        .key(KEY_MUTE)
        .ghost()
        .tooltip(if active.muted { "Unmute" } else { "Mute" });

    let pos = active.stream.time_pos().ok().flatten().unwrap_or_default();
    let dur = active.stream.duration().ok().flatten().unwrap_or_default();
    let time_label = text(format!("{} / {}", format_time(pos), format_time(dur)))
        .muted()
        .font_size(tokens::TEXT_XS.size);

    let scrub = slider(active.scrub_value, tokens::PRIMARY)
        .key(KEY_SCRUB)
        .width(Size::Fill(1.0));

    row([play_btn, time_label, scrub, mute_btn])
        .gap(tokens::SPACE_2)
        .align(Align::Center)
        .width(Size::Fill(1.0))
}
