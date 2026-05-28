use std::{
    collections::HashMap,
    sync::LazyLock,
    time::{Duration, Instant},
};

use aetna_core::{prelude::*, surface::SurfaceAlpha};
use rumble_protocol::proto::RelayFileSharePayload;

use crate::animated_gpu::AnimatedGpu;

use super::{format_size, gif_lightbox_key, gif_play_key, preview_key};

/// Decoded image cache, keyed by `transfer_id`. The App maintains this
/// map and passes it through on render so the chat can swap a file
/// card for an inline preview when the underlying transfer is complete.
///
/// Static images carry a single decoded [`Image`]. Animated GIFs carry
/// the full sequence of frames so playback runs without re-decoding.
pub type ImageCache = HashMap<String, CachedImage>;

/// GPU mirrors for animated previews, keyed by `transfer_id`. The App
/// owns these (allocates + uploads in `before_paint`) and threads the
/// map through `render` so [`image_preview`] can pick `surface()` for
/// any animated entry whose mirror is live. Static images and
/// animated entries that haven't been mirrored yet (one-frame race
/// after decode) fall back to the `image()` widget.
pub type AnimatedTextureMap = HashMap<String, AnimatedGpu>;

/// Decoded poster thumbnails for downloaded videos, keyed by
/// `transfer_id`. The App's `pump_video_thumbs` populates this via
/// a one-shot libmpv decode of the first frame; chat consults it to
/// swap a `file_offer_card` for a `video_preview` card.
pub type VideoThumbMap = HashMap<String, Image>;

/// Per-message GIF playback state, keyed by `transfer_id`. Lives on the
/// App separate from the cache so a re-decode (e.g. cache eviction +
/// repopulation) doesn't reset playback position. Static images don't
/// need an entry — the absence of a key is "n/a".
#[derive(Clone, Debug)]
pub struct GifPlayback {
    /// Index into the animated cache entry's `frames` vec.
    pub frame_idx: usize,
    /// Wall-clock instant when `frame_idx` was last advanced. The pump
    /// computes `now - last_advance` against the current frame's delay
    /// to decide whether to advance.
    pub last_advance: Instant,
    /// Whether the animation is currently advancing. The play/pause
    /// icon flips this; the chat-settings auto-play default seeds it
    /// at first observation.
    pub playing: bool,
}

impl GifPlayback {
    pub fn new(playing: bool) -> Self {
        Self {
            frame_idx: 0,
            last_advance: Instant::now(),
            playing,
        }
    }
}

/// Time until the currently-displayed animated frame should be replaced
/// by the next one. Fed to aetna's `redraw_within` on animated
/// `surface()` Els so the host schedules a wake-up at exactly the
/// moment the GIF wants to advance.
///
/// Returns `None` for paused playbacks.
pub(super) fn next_frame_deadline(frames: &[(Image, Duration)], pb: &GifPlayback) -> Option<Duration> {
    if !pb.playing || frames.is_empty() {
        return None;
    }
    let idx = pb.frame_idx.min(frames.len() - 1);
    let cur_delay = frames[idx].1;
    let elapsed = Instant::now().saturating_duration_since(pb.last_advance);
    Some(cur_delay.saturating_sub(elapsed))
}

/// One entry in the chat image cache. `Static` is a single decoded
/// frame; `Animated` carries every frame of a GIF along with the
/// per-frame display delay so the App's pump can advance an
/// independent playhead per message; `Svg` is a parsed vector asset
/// rendered natively by aetna at any size.
#[derive(Clone, Debug)]
pub enum CachedImage {
    Static(Image),
    Animated {
        /// Decoded frames in source order, paired with each frame's
        /// display duration. Always at least 2 entries — single-frame
        /// GIFs collapse to `Static` at decode time.
        frames: Vec<(Image, Duration)>,
    },
    /// Parsed SVG. Identity-hashed inside `SvgIcon` so cache lookups
    /// dedup; clone is a cheap `Arc` bump.
    Svg(SvgIcon),
}

impl CachedImage {
    pub fn is_animated(&self) -> bool {
        matches!(self, Self::Animated { .. })
    }

    pub fn is_vector(&self) -> bool {
        matches!(self, Self::Svg(_))
    }

    pub fn current_frame(&self, playback: Option<&GifPlayback>) -> Option<&Image> {
        match self {
            Self::Static(img) => Some(img),
            Self::Animated { frames, .. } => {
                let idx = playback.map(|p| p.frame_idx).unwrap_or(0);
                Some(&frames[idx.min(frames.len() - 1)].0)
            }
            Self::Svg(_) => None,
        }
    }

    pub fn current_frame_size(&self, playback: Option<&GifPlayback>) -> (u32, u32) {
        match self {
            Self::Static(img) => (img.width(), img.height()),
            Self::Animated { frames, .. } => {
                let idx = playback.map(|p| p.frame_idx).unwrap_or(0);
                let frame = &frames[idx.min(frames.len() - 1)].0;
                (frame.width(), frame.height())
            }
            Self::Svg(icon) => {
                let [_, _, w, h] = icon.vector_asset().view_box;
                (w.round().max(1.0) as u32, h.round().max(1.0) as u32)
            }
        }
    }
}

// Player-control SVG icons. Loaded once via `parse_current_color` so
// `.text_color(...)` tints them.
static SVG_PLAY: LazyLock<SvgIcon> =
    LazyLock::new(|| SvgIcon::parse_current_color(include_str!("../../assets/icons/play.svg")).expect("play.svg parses"));
static SVG_PAUSE: LazyLock<SvgIcon> = LazyLock::new(|| {
    SvgIcon::parse_current_color(include_str!("../../assets/icons/pause.svg")).expect("pause.svg parses")
});
static SVG_LIGHTBOX_OPEN: LazyLock<SvgIcon> = LazyLock::new(|| {
    SvgIcon::parse_current_color(include_str!("../../assets/icons/lightbox_open.svg")).expect("lightbox_open.svg parses")
});

/// Returns true if `name`'s extension is one we can decode for inline
/// previews. Mirrors the `image` crate features compiled into the
/// aetna client (png/jpg/jpeg/gif/webp/bmp/ico/tif/tiff).
pub fn is_image_name(name: &str) -> bool {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    matches!(
        ext.as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "tif" | "tiff" | "svg"
    )
}

/// True when the file extension marks an SVG.
pub fn is_svg_name(name: &str) -> bool {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    ext == "svg"
}

/// Wrap an inner El (`image()`, `surface()`, or `stack([poster, badge])`)
/// in the nested-`Size::Aspect` Contain layout the three preview cards
/// share. Centers a natural-aspect rect inside a fill-width column,
/// capped to `max_height` (and optionally `max_width`).
fn preview_card(inner: El, natural_w: u32, natural_h: u32, max_height: f32, max_width: Option<f32>) -> El {
    let aspect_h_over_w = natural_h as f32 / natural_w.max(1) as f32;
    let aspect_w_over_h = natural_w.max(1) as f32 / natural_h.max(1) as f32;
    let inner_wrapper = column([inner])
        .height(Size::Fill(1.0))
        .width(Size::Aspect(aspect_w_over_h));
    let mut outer = column([inner_wrapper])
        .width(Size::Fill(1.0))
        .height(Size::Aspect(aspect_h_over_w))
        .max_height(max_height)
        .align(Align::Center);
    if let Some(mw) = max_width {
        outer = outer.max_width(mw);
    }
    outer
}

/// Inline thumbnail card for an image attachment whose underlying
/// transfer is complete.
pub(super) fn image_preview(
    offer: &RelayFileSharePayload,
    cached: &CachedImage,
    playback: Option<&GifPlayback>,
    gpu: Option<&AnimatedGpu>,
) -> El {
    const PREVIEW_HEIGHT: f32 = 400.0;

    let preview_layer: El = match (cached, gpu) {
        (CachedImage::Animated { frames, .. }, Some(gpu)) => {
            let is_playing = playback.map(|p| p.playing).unwrap_or(false);
            let deadline = playback.and_then(|pb| next_frame_deadline(frames, pb));
            animated_surface_preview(&offer.transfer_id, gpu, is_playing, deadline, PREVIEW_HEIGHT)
        }
        (CachedImage::Svg(icon), _) => svg_contain_preview(icon.clone(), PREVIEW_HEIGHT),
        _ => {
            let frame = cached
                .current_frame(playback)
                .expect("non-svg variant always has a raster frame")
                .clone();
            let (nw, nh) = cached.current_frame_size(playback);
            if cached.is_animated() {
                // Animated cache entry whose GPU mirror hasn't
                // materialized yet (one-frame race after decode).
                // Falls through to image()-based playback for this
                // frame; the next frame swaps to surface().
                let preview = image(frame)
                    .radius(tokens::RADIUS_SM)
                    .width(Size::Fill(1.0))
                    .height(Size::Fill(1.0));
                let is_playing = playback.map(|p| p.playing).unwrap_or(false);
                let inner = stack([preview, gif_controls_overlay(&offer.transfer_id, is_playing)])
                    .width(Size::Fill(1.0))
                    .height(Size::Fill(1.0));
                preview_card(inner, nw, nh, PREVIEW_HEIGHT, None)
            } else {
                // Static raster: image_fit(Contain) handles aspect
                // letterboxing on its own.
                let aspect_h_over_w = nh as f32 / nw.max(1) as f32;
                image(frame)
                    .image_fit(ImageFit::Contain)
                    .radius(tokens::RADIUS_SM)
                    .width(Size::Fill(1.0))
                    .height(Size::Aspect(aspect_h_over_w))
                    .max_height(PREVIEW_HEIGHT)
            }
        }
    };

    let caption = text(format!("{} · {}", offer.name, format_size(offer.size)))
        .muted()
        .font_size(tokens::TEXT_XS.size)
        .ellipsis()
        .key(format!("chat:preview:{}.caption", offer.transfer_id))
        .tooltip(offer.name.clone());

    column([preview_layer, caption])
        .key(preview_key(&offer.transfer_id))
        .focusable()
        .cursor(Cursor::Pointer)
        .gap(tokens::SPACE_1)
        .padding(Sides::all(tokens::SPACE_2))
        .fill(tokens::SECONDARY)
        .stroke(tokens::BORDER)
        .stroke_width(1.0)
        .radius(tokens::RADIUS_MD)
        .width(Size::Fill(1.0))
}

fn svg_contain_preview(icon: SvgIcon, height: f32) -> El {
    let [_, _, vw, vh] = icon.vector_asset().view_box;
    let nat_w = vw.round().max(1.0) as u32;
    let nat_h = vh.round().max(1.0) as u32;
    let asset = icon.vector_asset().clone();

    let inner = vector(asset).height(Size::Fill(1.0)).width(Size::Fill(1.0));
    preview_card(inner, nat_w, nat_h, height, None).radius(tokens::RADIUS_SM)
}

/// Fit-to-window SVG variant for the lightbox body.
///
/// Unlike [`svg_contain_preview`] we can't swap the layout override for
/// the nested-`Size::Aspect` pattern here: that pattern needs a static
/// cap on at least one axis to anchor the outer rect, and the lightbox
/// body's actual size is only known at layout time.
pub(super) fn svg_contain_fill(icon: SvgIcon) -> El {
    let [_, _, vw, vh] = icon.vector_asset().view_box;
    let nat_w = vw.max(1.0);
    let nat_h = vh.max(1.0);
    let asset = icon.vector_asset().clone();
    let vec_el = vector(asset).width(Size::Fixed(nat_w)).height(Size::Fixed(nat_h));
    stack([vec_el])
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0))
        .radius(tokens::RADIUS_MD)
        .layout(move |ctx: LayoutCtx| {
            let cw = ctx.container.w.max(0.0);
            let ch = ctx.container.h.max(0.0);
            if cw <= 0.0 || ch <= 0.0 {
                return vec![Rect::new(ctx.container.x, ctx.container.y, 0.0, 0.0)];
            }
            let scale = (cw / nat_w).min(ch / nat_h);
            let w = nat_w * scale;
            let h = nat_h * scale;
            let x = ctx.container.x + (cw - w) * 0.5;
            let y = ctx.container.y + (ch - h) * 0.5;
            vec![Rect::new(x, y, w, h)]
        })
}

/// Inline preview card for a downloaded video: poster thumbnail
/// (first frame extracted via libmpv) with a centered play overlay.
pub(super) fn video_preview(offer: &RelayFileSharePayload, thumb: &Image) -> El {
    const PREVIEW_HEIGHT: f32 = 400.0;

    let poster = image(thumb.clone())
        .radius(tokens::RADIUS_SM)
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0));

    let play_badge = stack([icon(SVG_PLAY.clone())
        .text_color(tokens::FOREGROUND)
        .width(Size::Fixed(36.0))
        .height(Size::Fixed(36.0))])
    .padding(Sides::all(tokens::SPACE_3))
    .fill(tokens::OVERLAY_SCRIM)
    .radius(tokens::RADIUS_PILL);

    let centred_badge = column([
        spacer().height(Size::Fill(1.0)),
        row([
            spacer().width(Size::Fill(1.0)),
            play_badge,
            spacer().width(Size::Fill(1.0)),
        ])
        .align(Align::Center)
        .width(Size::Fill(1.0)),
        spacer().height(Size::Fill(1.0)),
    ])
    .width(Size::Fill(1.0))
    .height(Size::Fill(1.0));

    let inner = stack([poster, centred_badge])
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0));
    let preview_layer = preview_card(inner, thumb.width(), thumb.height(), PREVIEW_HEIGHT, None);

    let caption = text(format!("{} · {}", offer.name, format_size(offer.size)))
        .muted()
        .font_size(tokens::TEXT_XS.size)
        .ellipsis()
        .key(format!("chat:video-preview:{}.caption", offer.transfer_id))
        .tooltip(offer.name.clone());

    column([preview_layer, caption])
        .key(crate::video::open_video_key(&offer.transfer_id))
        .focusable()
        .cursor(Cursor::Pointer)
        .gap(tokens::SPACE_1)
        .padding(Sides::all(tokens::SPACE_2))
        .fill(tokens::SECONDARY)
        .stroke(tokens::BORDER)
        .stroke_width(1.0)
        .radius(tokens::RADIUS_MD)
        .width(Size::Fill(1.0))
}

/// Build the surface-based preview for an animated entry.
fn animated_surface_preview(
    transfer_id: &str,
    gpu: &AnimatedGpu,
    is_playing: bool,
    next_frame_in: Option<Duration>,
    preview_height: f32,
) -> El {
    /// Cap on the inscribed surface width.
    const PREVIEW_MAX_WIDTH: f32 = 480.0;

    let (tw, th) = gpu.size();

    let mut surface_el = surface(gpu.app_texture().clone())
        .surface_alpha(SurfaceAlpha::Straight)
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0))
        .radius(tokens::RADIUS_SM);
    if let Some(deadline) = next_frame_in {
        surface_el = surface_el.redraw_within(deadline);
    }

    let inner = stack([surface_el, gif_controls_overlay(transfer_id, is_playing)])
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0));
    preview_card(inner, tw, th, preview_height, Some(PREVIEW_MAX_WIDTH))
}

/// Bottom-right pill of icon buttons overlaid on an animated image preview.
fn gif_controls_overlay(transfer_id: &str, is_playing: bool) -> El {
    let play_icon: IconSource = if is_playing {
        SVG_PAUSE.clone().into()
    } else {
        SVG_PLAY.clone().into()
    };
    let play_btn = icon_button(play_icon)
        .key(gif_play_key(transfer_id))
        .ghost()
        .tooltip(if is_playing { "Pause" } else { "Play" });
    let lightbox_btn = icon_button(SVG_LIGHTBOX_OPEN.clone())
        .key(gif_lightbox_key(transfer_id))
        .ghost()
        .tooltip("Open lightbox");

    let mut pill = row([play_btn, lightbox_btn])
        .gap(tokens::SPACE_1)
        .padding(Sides::all(tokens::SPACE_1))
        .fill(tokens::OVERLAY_SCRIM)
        .radius(tokens::RADIUS_SM);

    // While playing the pill hides at rest and fades in on hover.
    // While paused the pill stays visible so the affordance is always discoverable.
    if is_playing {
        pill = pill.hover_alpha(0.0, 1.0);
    }

    column([
        spacer().height(Size::Fill(1.0)),
        row([spacer().width(Size::Fill(1.0)), pill])
            .align(Align::End)
            .width(Size::Fill(1.0)),
    ])
    .padding(Sides::all(tokens::SPACE_2))
    .width(Size::Fill(1.0))
    .height(Size::Fill(1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_image_name_accepts_svg() {
        assert!(is_image_name("logo.svg"));
        assert!(is_image_name("Logo.SVG"));
        assert!(is_svg_name("logo.svg"));
        assert!(!is_svg_name("logo.png"));
    }

    #[test]
    fn cached_image_svg_reports_view_box_as_natural_size() {
        let icon = SvgIcon::parse(
            r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 64"><rect width="32" height="64" fill="red"/></svg>"##,
        )
        .expect("parse");
        let cached = CachedImage::Svg(icon);
        assert!(cached.is_vector());
        assert!(!cached.is_animated());
        assert!(cached.current_frame(None).is_none());
        assert_eq!(cached.current_frame_size(None), (32, 64));
    }
}
