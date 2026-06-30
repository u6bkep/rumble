use damascene_core::{prelude::*, surface::SurfaceAlpha};

use crate::animated_gpu::AnimatedGpu;

use super::{
    KEY_LIGHTBOX_CLOSE, KEY_LIGHTBOX_DISMISS, KEY_LIGHTBOX_IMAGE, KEY_LIGHTBOX_ZOOM_FIT, KEY_LIGHTBOX_ZOOM_NATURAL,
    LIGHTBOX_ZOOM_MAX, LIGHTBOX_ZOOM_MIN,
    image_preview::{CachedImage, GifPlayback, next_frame_deadline},
};

/// State for the image lightbox. Keyed by `transfer_id` so a re-render
/// looks the image back up out of `MediaCache` — that means a transfer
/// being garbage-collected mid-view dismisses the overlay cleanly the
/// next frame instead of leaving a stale clone behind.
///
/// Pan/zoom is deliberately *not* stored here: the body is a damascene
/// [`viewport()`](damascene_core::tree::viewport), which owns the
/// pan/zoom transform (persisted by its `.key(...)` in `UiState`,
/// hit-test / chrome scaling baked into layout). The app only nudges it
/// with one-shot [`ViewportRequest`](damascene_core::viewport::ViewportRequest)s
/// — fit on open / resize, reset on "100%" — see [`LightboxViewReq`].
#[derive(Clone, Debug)]
pub struct Lightbox {
    pub transfer_id: String,
    pub name: String,
}

impl Lightbox {
    pub fn new(transfer_id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            transfer_id: transfer_id.into(),
            name: name.into(),
        }
    }
}

/// A one-shot view change the app wants applied to the lightbox
/// `viewport()` next frame. The App translates it into a
/// [`ViewportRequest`](damascene_core::viewport::ViewportRequest) in
/// `drain_viewport_requests`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LightboxViewReq {
    /// Frame the image to fit the body — on open, on window resize, and
    /// on the "Fit" button.
    Fit,
    /// Snap back to the 1:1, centered framing — the "100%" button.
    Reset,
}

/// Build the click-to-enlarge image viewer overlay. Pan/zoom is handled
/// natively by the body [`viewport()`](damascene_core::tree::viewport)
/// (drag to pan, wheel to zoom under the cursor); the App drives fit /
/// reset via [`LightboxViewReq`].
pub fn render_lightbox(
    lightbox: &Lightbox,
    cached: &CachedImage,
    playback: Option<&GifPlayback>,
    gpu: Option<&AnimatedGpu>,
) -> El {
    let header = lightbox_header(lightbox);
    let body = lightbox_body(cached, playback, gpu);

    let panel = column([header, body])
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

    // The panel sits inside a padded container so it leaves visible
    // gutters on every side. Clicks on the transparent container land
    // on the scrim sibling underneath, dismissing the overlay.
    let centered = stack([panel]).fill_size().padding(Sides::all(tokens::SPACE_7));

    overlay([scrim(KEY_LIGHTBOX_DISMISS), centered])
}

fn lightbox_header(lightbox: &Lightbox) -> El {
    // Wheel-zoom is cursor-anchored and continuous, so there are no
    // per-step +/− buttons — "Fit" reframes to the body and "100%"
    // snaps back to 1:1.
    row([
        text(lightbox.name.clone())
            .title()
            .ellipsis()
            .key("chat:lightbox:title")
            .tooltip(lightbox.name.clone()),
        spacer(),
        button("Fit").key(KEY_LIGHTBOX_ZOOM_FIT).ghost(),
        button("100%").key(KEY_LIGHTBOX_ZOOM_NATURAL).ghost(),
        button("Close").key(KEY_LIGHTBOX_CLOSE),
    ])
    .gap(tokens::SPACE_2)
    .align(Align::Center)
    .width(Size::Fill(1.0))
}

fn lightbox_body(cached: &CachedImage, playback: Option<&GifPlayback>, gpu: Option<&AnimatedGpu>) -> El {
    let (natural_w, natural_h) = {
        let (w, h) = cached.current_frame_size(playback);
        (w as f32, h as f32)
    };

    // The image/surface/vector child is laid out at its natural pixel
    // size in the viewport's untransformed content space; the viewport's
    // pan/zoom transform (and `FitContent`) does all the scaling, so
    // there are no per-content-type scale/translate branches here.
    let inner = match (cached, gpu) {
        (CachedImage::Animated { frames, .. }, Some(gpu)) => {
            let mut el = surface(gpu.app_texture().clone())
                .surface_alpha(SurfaceAlpha::Straight)
                .width(Size::Fixed(natural_w))
                .height(Size::Fixed(natural_h))
                .radius(tokens::RADIUS_MD);
            if let Some(deadline) = playback.and_then(|pb| next_frame_deadline(frames, pb)) {
                el = el.redraw_within(deadline);
            }
            el
        }
        (CachedImage::Svg(icon), _) => vector(icon.vector_asset().clone())
            .width(Size::Fixed(natural_w))
            .height(Size::Fixed(natural_h))
            .radius(tokens::RADIUS_MD),
        _ => {
            let frame = cached
                .current_frame(playback)
                .expect("non-svg lightbox path always has a raster frame")
                .clone();
            image(frame)
                .width(Size::Fixed(natural_w))
                .height(Size::Fixed(natural_h))
                .radius(tokens::RADIUS_MD)
        }
    };

    // `PanBounds::Contain` keeps the image from being dragged off into an
    // empty gutter — the right bound for a fit-to-view image (and a free
    // fix for the old unbounded-pan bug). `no_hover()` because the
    // viewport fill is canvas chrome, not an interactive surface.
    viewport([inner])
        .key(KEY_LIGHTBOX_IMAGE)
        .min_zoom(LIGHTBOX_ZOOM_MIN)
        .max_zoom(LIGHTBOX_ZOOM_MAX)
        .pan_bounds(damascene_core::viewport::PanBounds::Contain)
        .no_hover()
        .fill(tokens::CARD)
        .radius(tokens::RADIUS_MD)
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0))
}
