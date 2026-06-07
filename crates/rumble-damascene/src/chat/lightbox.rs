use damascene_core::{prelude::*, surface::SurfaceAlpha};

use crate::animated_gpu::AnimatedGpu;

use super::{
    KEY_LIGHTBOX_CLOSE, KEY_LIGHTBOX_DISMISS, KEY_LIGHTBOX_IMAGE, KEY_LIGHTBOX_ZOOM_FIT, KEY_LIGHTBOX_ZOOM_IN,
    KEY_LIGHTBOX_ZOOM_NATURAL, KEY_LIGHTBOX_ZOOM_OUT, LIGHTBOX_ZOOM_MAX, LIGHTBOX_ZOOM_MIN, LIGHTBOX_ZOOM_STEP,
    image_preview::{CachedImage, GifPlayback, next_frame_deadline, svg_contain_fill},
};

/// State for the image lightbox. Keyed by `transfer_id` so a re-render
/// looks the image back up out of `ImageCache` — that means a transfer
/// being garbage-collected mid-view dismisses the overlay cleanly the
/// next frame instead of leaving a stale clone behind.
#[derive(Clone, Debug)]
pub struct Lightbox {
    pub transfer_id: String,
    pub name: String,
    /// Explicit zoom factor relative to decoded image pixels. `1.0`
    /// paints the image at natural size. Ignored while
    /// `fit_to_window` is true.
    pub zoom: f32,
    /// Fit the image into the available lightbox body. This is kept as
    /// a separate mode so the zoom label never pretends the fit scale
    /// is a source-pixel percentage without knowing the body rect.
    pub fit_to_window: bool,
    /// Pan offset in logical pixels. `(0, 0)` keeps the (scaled) image
    /// centred in the body.
    pub pan: (f32, f32),
    /// Ephemeral pan-drag state, cleared on `PointerUp`. Captured at
    /// `PointerDown`: the pointer position + the pan value at that
    /// moment, so subsequent `Drag` events can compute a delta without
    /// per-event accumulation drift.
    pub drag: PanDrag,
}

impl Lightbox {
    pub fn new(transfer_id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            transfer_id: transfer_id.into(),
            name: name.into(),
            zoom: 1.0,
            fit_to_window: true,
            pan: (0.0, 0.0),
            drag: PanDrag::default(),
        }
    }

    pub fn fit(&mut self) {
        self.fit_to_window = true;
        self.pan = (0.0, 0.0);
    }

    pub fn natural_size(&mut self) {
        self.fit_to_window = false;
        self.zoom = 1.0;
        self.pan = (0.0, 0.0);
    }

    pub fn zoom_in(&mut self, body_size: Option<(f32, f32)>, image_size: Option<(u32, u32)>) {
        if self.fit_to_window {
            self.set_zoom(fit_step_zoom(body_size, image_size, ZoomDirection::In));
            return;
        }
        self.set_zoom(next_zoom_step(self.zoom, ZoomDirection::In));
    }

    pub fn zoom_out(&mut self, body_size: Option<(f32, f32)>, image_size: Option<(u32, u32)>) {
        if self.fit_to_window {
            self.set_zoom(fit_step_zoom(body_size, image_size, ZoomDirection::Out));
            return;
        }
        self.set_zoom(next_zoom_step(self.zoom, ZoomDirection::Out));
    }

    pub fn set_zoom(&mut self, zoom: f32) {
        self.fit_to_window = false;
        self.zoom = zoom.clamp(LIGHTBOX_ZOOM_MIN, LIGHTBOX_ZOOM_MAX);
        // At natural size the image is anchored centre by the lightbox
        // body stack, so residual pan would just bias the framing.
        if (self.zoom - 1.0).abs() < f32::EPSILON {
            self.pan = (0.0, 0.0);
        }
    }

    /// True when the scaled image (`natural * zoom`) exceeds the body
    /// in either axis — i.e. there's content outside the viewport.
    /// Drives both pan-drag enablement and the grab cursor.
    pub fn image_overflows_body(&self, natural: (f32, f32), body: Option<(f32, f32)>) -> bool {
        if self.fit_to_window {
            return false;
        }
        let Some((body_w, body_h)) = body else {
            return false;
        };
        let scaled_w = natural.0 * self.zoom;
        let scaled_h = natural.1 * self.zoom;
        scaled_w > body_w || scaled_h > body_h
    }
}

/// Ephemeral pan-drag state: anchor pointer + pan value at
/// `PointerDown`, used by `Drag` to compute the new pan as
/// `start_pan + (current_pointer - anchor_pointer)`.
#[derive(Clone, Debug, Default)]
pub struct PanDrag {
    pub anchor: Option<((f32, f32), (f32, f32))>,
}

#[derive(Clone, Copy, Debug)]
pub enum ZoomDirection {
    In,
    Out,
}

fn fit_step_zoom(body_size: Option<(f32, f32)>, image_size: Option<(u32, u32)>, direction: ZoomDirection) -> f32 {
    fit_scale(body_size, image_size)
        .map(|scale| next_zoom_step(scale, direction))
        .unwrap_or_else(|| next_zoom_step(1.0, direction))
}

fn fit_scale(body_size: Option<(f32, f32)>, image_size: Option<(u32, u32)>) -> Option<f32> {
    let ((body_w, body_h), (image_w, image_h)) = (body_size?, image_size?);
    if body_w <= 0.0 || body_h <= 0.0 || image_w == 0 || image_h == 0 {
        return None;
    }
    Some((body_w / image_w as f32).min(body_h / image_h as f32))
}

fn next_zoom_step(zoom: f32, direction: ZoomDirection) -> f32 {
    let step = LIGHTBOX_ZOOM_STEP;
    let stepped = match direction {
        ZoomDirection::In => {
            let next = (zoom / step).ceil() * step;
            if (next - zoom).abs() < f32::EPSILON {
                next + step
            } else {
                next
            }
        }
        ZoomDirection::Out => {
            let next = (zoom / step).floor() * step;
            if (next - zoom).abs() < f32::EPSILON {
                next - step
            } else {
                next
            }
        }
    };
    stepped.clamp(LIGHTBOX_ZOOM_MIN, LIGHTBOX_ZOOM_MAX)
}

/// Build the click-to-enlarge image viewer overlay.
///
/// `body_size` is the lightbox body's rect from the previous frame's
/// layout — the App reads it via [`BuildCx::rect_of_key`] on
/// [`KEY_LIGHTBOX_IMAGE`] and threads it in. It only drives the
/// render-time grab-cursor decision; one frame stale is fine there.
/// Event-time zoom uses the live `EventCx::rect_of_key` answer instead.
pub fn render_lightbox(
    lightbox: &Lightbox,
    body_size: Option<(f32, f32)>,
    cached: &CachedImage,
    playback: Option<&GifPlayback>,
    gpu: Option<&AnimatedGpu>,
) -> El {
    let header = lightbox_header(lightbox);
    let body = lightbox_body(lightbox, body_size, cached, playback, gpu);

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
    let zoom_pct = (lightbox.zoom * 100.0).round() as i32;
    let zoom_label = if lightbox.fit_to_window {
        "Fit".to_string()
    } else {
        format!("{zoom_pct}%")
    };

    let mut zoom_out = button("−").key(KEY_LIGHTBOX_ZOOM_OUT).ghost();
    if !lightbox.fit_to_window && lightbox.zoom <= LIGHTBOX_ZOOM_MIN + f32::EPSILON {
        zoom_out = zoom_out.disabled();
    }
    let mut zoom_in = button("+").key(KEY_LIGHTBOX_ZOOM_IN).ghost();
    if !lightbox.fit_to_window && lightbox.zoom >= LIGHTBOX_ZOOM_MAX - f32::EPSILON {
        zoom_in = zoom_in.disabled();
    }
    let mut fit = button("Fit").key(KEY_LIGHTBOX_ZOOM_FIT).ghost();
    if lightbox.fit_to_window && lightbox.pan == (0.0, 0.0) {
        fit = fit.disabled();
    }
    let mut natural = button("100%").key(KEY_LIGHTBOX_ZOOM_NATURAL).ghost();
    if !lightbox.fit_to_window && (lightbox.zoom - 1.0).abs() < f32::EPSILON && lightbox.pan == (0.0, 0.0) {
        natural = natural.disabled();
    }

    row([
        text(lightbox.name.clone())
            .title()
            .ellipsis()
            .key("chat:lightbox:title")
            .tooltip(lightbox.name.clone()),
        spacer(),
        zoom_out,
        text(zoom_label)
            .muted()
            .font_size(tokens::TEXT_SM.size)
            .width(Size::Fixed(56.0))
            .text_align(TextAlign::Center),
        zoom_in,
        fit,
        natural,
        button("Close").key(KEY_LIGHTBOX_CLOSE),
    ])
    .gap(tokens::SPACE_2)
    .align(Align::Center)
    .width(Size::Fill(1.0))
}

fn lightbox_body(
    lightbox: &Lightbox,
    body_size: Option<(f32, f32)>,
    cached: &CachedImage,
    playback: Option<&GifPlayback>,
    gpu: Option<&AnimatedGpu>,
) -> El {
    let (natural_w, natural_h) = {
        let (w, h) = cached.current_frame_size(playback);
        (w as f32, h as f32)
    };
    let fit_to_window = lightbox.fit_to_window;
    // Previous frame's laid-out body size, used to decide the grab cursor.
    let prev_body_size = body_size;

    let inner = match (cached, gpu) {
        (CachedImage::Animated { frames, .. }, Some(gpu)) => {
            let mut el = surface(gpu.app_texture().clone()).surface_alpha(SurfaceAlpha::Straight);
            if lightbox.fit_to_window {
                el = el
                    .surface_fit(ImageFit::Contain)
                    .width(Size::Fill(1.0))
                    .height(Size::Fill(1.0));
            } else {
                el = el
                    .width(Size::Fixed(natural_w))
                    .height(Size::Fixed(natural_h))
                    .scale(lightbox.zoom)
                    .translate(lightbox.pan.0, lightbox.pan.1);
            }
            el = el.radius(tokens::RADIUS_MD);
            if let Some(deadline) = playback.and_then(|pb| next_frame_deadline(frames, pb)) {
                el = el.redraw_within(deadline);
            }
            el
        }
        (CachedImage::Svg(icon), _) => {
            if lightbox.fit_to_window {
                svg_contain_fill(icon.clone())
            } else {
                vector(icon.vector_asset().clone())
                    .width(Size::Fixed(natural_w))
                    .height(Size::Fixed(natural_h))
                    .radius(tokens::RADIUS_MD)
                    .scale(lightbox.zoom)
                    .translate(lightbox.pan.0, lightbox.pan.1)
            }
        }
        _ => {
            let frame = cached
                .current_frame(playback)
                .expect("non-svg lightbox path always has a raster frame")
                .clone();
            let mut el = image(frame).radius(tokens::RADIUS_MD);
            if lightbox.fit_to_window {
                el = el
                    .image_fit(ImageFit::Contain)
                    .width(Size::Fill(1.0))
                    .height(Size::Fill(1.0));
            } else {
                el = el
                    .width(Size::Fixed(natural_w))
                    .height(Size::Fixed(natural_h))
                    .scale(lightbox.zoom)
                    .translate(lightbox.pan.0, lightbox.pan.1);
            }
            el
        }
    };

    stack([inner])
        .key(KEY_LIGHTBOX_IMAGE)
        .layout(move |ctx: LayoutCtx| {
            if fit_to_window {
                return vec![ctx.container];
            }

            let x = ctx.container.x + (ctx.container.w - natural_w) * 0.5;
            let y = ctx.container.y + (ctx.container.h - natural_h) * 0.5;
            vec![Rect::new(x, y, natural_w, natural_h)]
        })
        .clip()
        .fill(tokens::CARD)
        .radius(tokens::RADIUS_MD)
        .cursor(if lightbox.drag.anchor.is_some() {
            Cursor::Grabbing
        } else if lightbox.image_overflows_body((natural_w, natural_h), prev_body_size) {
            Cursor::Grab
        } else {
            Cursor::Default
        })
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_zoom_buttons_step_from_current_fit_scale() {
        let large_body = Some((900.0, 600.0));
        let small_image = Some((300, 200));
        let large_fit = fit_scale(large_body, small_image).expect("fit scale");
        assert!((large_fit - 3.0).abs() < f32::EPSILON);
        assert!((next_zoom_step(large_fit, ZoomDirection::In) - 3.2).abs() < 0.001);
        assert!((next_zoom_step(large_fit, ZoomDirection::Out) - 2.8).abs() < 0.001);

        let uneven_fit = fit_scale(Some((111.0, 100.0)), small_image).expect("fit scale");
        assert!((uneven_fit - 0.37).abs() < 0.001);
        assert!((next_zoom_step(uneven_fit, ZoomDirection::In) - 0.4).abs() < 0.001);
        assert_eq!(next_zoom_step(uneven_fit, ZoomDirection::Out), LIGHTBOX_ZOOM_MIN);
    }

    #[test]
    fn pan_enabled_when_scaled_image_overflows_body() {
        let body = Some((800.0, 600.0));
        let huge = (4000.0, 3000.0);
        let small = (200.0, 150.0);

        // Fit mode is always non-overflowing (Contain guarantees the image fits).
        let lb = Lightbox {
            transfer_id: "t".into(),
            name: "n".into(),
            zoom: 1.0,
            fit_to_window: true,
            pan: (0.0, 0.0),
            drag: PanDrag::default(),
        };
        assert!(!lb.image_overflows_body(huge, body));

        // Large image at <100%: scaled is 4000*0.3 = 1200 > 800 → pannable.
        let lb = Lightbox {
            fit_to_window: false,
            zoom: 0.3,
            ..lb
        };
        assert!(lb.image_overflows_body(huge, body));

        // Small image at 200% still fits: 200*2 = 400 < 800 → not pannable.
        let lb = Lightbox { zoom: 2.0, ..lb };
        assert!(!lb.image_overflows_body(small, body));

        // Body unknown → no pan (matches first-frame behavior).
        assert!(!lb.image_overflows_body(huge, None));
    }
}
