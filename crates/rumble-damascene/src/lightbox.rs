//! Media lightbox coordinator. The image, video, and 3D-model lightboxes
//! share one overlay slot and are mutually exclusive — opening any one
//! closes the other two. This module owns their combined state and the
//! event dispatch (open detection, image zoom/pan, video controls,
//! close). The collaborator-heavy *open* lifecycle (path lookup, image
//! decode, libmpv spawn) stays on the App and runs in response to the
//! [`LightboxAction`] this returns.

use damascene_core::prelude::*;
use tokio::task::JoinHandle;

use crate::{app::PendingVideoOpenResult, chat, media_cache::MediaCache, model, video};

/// Combined state of the three mutually-exclusive lightboxes.
#[derive(Default)]
pub struct Lightboxes {
    /// Click-to-enlarge image viewer (incl. animated GIF playback).
    pub image: Option<chat::Lightbox>,
    /// Active video lightbox (libmpv-backed).
    pub video: Option<video::ActiveVideo>,
    /// In-flight `VideoStream::open` task; promoted to `video` by the
    /// App's `poll_video_open` when it finishes.
    pub pending_video_open: Option<JoinHandle<PendingVideoOpenResult>>,
    /// Orbit viewer for a downloaded 3D model.
    pub model: Option<model::ActiveModel>,
}

impl Lightboxes {
    /// Close the image lightbox and drop its full-res decode.
    pub fn close_image(&mut self, media_cache: &mut MediaCache) {
        self.image = None;
        media_cache.close_lightbox();
    }

    /// Tear down the video lightbox and abort any in-flight open.
    pub fn close_video(&mut self) {
        self.video = None;
        if let Some(handle) = self.pending_video_open.take() {
            handle.abort();
        }
    }

    /// Close the model lightbox.
    pub fn close_model(&mut self) {
        self.model = None;
    }
}

/// What [`handle_event`] decided the App should do. The open variants
/// carry the transfer id; the App runs its existing `open_*` methods,
/// which perform the collaborator-heavy work and close the other two.
pub enum LightboxAction {
    /// Not a lightbox event — continue the App's event cascade.
    Ignored,
    /// Consumed; the in-place state mutation already happened.
    Handled,
    OpenImage(String),
    OpenVideo(String),
    OpenModel(String),
    ToggleGif(String),
}

/// Dispatch a UI event to whichever lightbox owns it. Returns
/// [`LightboxAction::Ignored`] for non-lightbox events so the App keeps
/// cascading.
pub fn handle_event(
    lb: &mut Lightboxes,
    event: &UiEvent,
    cx: &EventCx,
    media_cache: &mut MediaCache,
) -> LightboxAction {
    // Per-GIF play/pause and explicit-open-lightbox icons live on
    // top of the preview card (`stack([preview, controls])`), so
    // their routes win over the underlying `chat:preview:*` route.
    // Handle them before the body click below.
    if matches!(event.kind, UiEventKind::Click | UiEventKind::Activate)
        && let Some(route) = event.route()
    {
        if let Some(transfer_id) = chat::parse_gif_play_key(route) {
            return LightboxAction::ToggleGif(transfer_id.to_string());
        }
        if let Some(transfer_id) = chat::parse_gif_lightbox_key(route) {
            return LightboxAction::OpenImage(transfer_id.to_string());
        }
    }

    // Image lightbox. Open by clicking (or keyboard-activating) an
    // inline image preview; close via the panel's Close button, the
    // scrim, or Escape. Open/close are stateless beyond toggling
    // `image` — the panel re-reads the image from
    // `image_cache` each frame so a transfer evicted out from under
    // an open lightbox dismisses it cleanly.
    if event.kind == UiEventKind::Click
        && let Some(route) = event.route()
        && let Some(transfer_id) = chat::parse_preview_key(route)
    {
        return LightboxAction::OpenImage(transfer_id.to_string());
    }
    if lb.image.is_some()
        && (event.is_click_or_activate(chat::KEY_LIGHTBOX_CLOSE)
            || (event.is_route(chat::KEY_LIGHTBOX_DISMISS) && event.kind == UiEventKind::Click)
            || event.kind == UiEventKind::Escape)
    {
        lb.close_image(media_cache);
        return LightboxAction::Handled;
    }
    let lightbox_image_size = lb.image.as_ref().and_then(|lightbox| {
        let cached = media_cache.lightbox_image_for(&lightbox.transfer_id)?;
        let playback = media_cache.gif_playback_for(&lightbox.transfer_id);
        Some(cached.current_frame_size(playback))
    });
    let lightbox_body_size = cx.rect_of_key(chat::KEY_LIGHTBOX_IMAGE).map(|r| (r.w, r.h));
    if let Some(lightbox) = lb.image.as_mut() {
        if event.is_click_or_activate(chat::KEY_LIGHTBOX_ZOOM_IN) {
            lightbox.zoom_in(lightbox_body_size, lightbox_image_size);
            return LightboxAction::Handled;
        }
        if event.is_click_or_activate(chat::KEY_LIGHTBOX_ZOOM_OUT) {
            lightbox.zoom_out(lightbox_body_size, lightbox_image_size);
            return LightboxAction::Handled;
        }
        if event.is_click_or_activate(chat::KEY_LIGHTBOX_ZOOM_FIT) {
            lightbox.fit();
            return LightboxAction::Handled;
        }
        if event.is_click_or_activate(chat::KEY_LIGHTBOX_ZOOM_NATURAL) {
            lightbox.natural_size();
            return LightboxAction::Handled;
        }
        // Drag-to-pan on the image surface. Gated on "scaled image
        // overflows the body" rather than `zoom > 1.0`: a large
        // image at <100% can still extend past the viewport, and
        // the user expects to be able to pan it in that case.
        let overflows = lightbox_image_size
            .is_some_and(|(w, h)| lightbox.image_overflows_body((w as f32, h as f32), lightbox_body_size));
        if event.route() == Some(chat::KEY_LIGHTBOX_IMAGE) && overflows {
            match event.kind {
                UiEventKind::PointerDown => {
                    if let Some(pos) = event.pointer {
                        lightbox.drag.anchor = Some((pos, lightbox.pan));
                    }
                    return LightboxAction::Handled;
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
                    return LightboxAction::Handled;
                }
                UiEventKind::PointerUp => {
                    lightbox.drag.anchor = None;
                    return LightboxAction::Handled;
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
        return LightboxAction::OpenVideo(transfer_id.to_string());
    }
    if lb.video.is_some()
        && (event.is_click_or_activate(video::KEY_LIGHTBOX_CLOSE)
            || (event.is_route(video::KEY_LIGHTBOX_DISMISS) && event.kind == UiEventKind::Click)
            || event.kind == UiEventKind::Escape)
    {
        lb.close_video();
        return LightboxAction::Handled;
    }
    if let Some(active) = lb.video.as_mut() {
        // Keyboard shortcuts: arrows (seek ±5s, +Shift =
        // ±30s), Home/End (seek to start/end), M (mute).
        // Space is handled separately via Activate routed to
        // the focused surface — damascene translates focused-
        // Space into Activate before KeyDown reaches us.
        if video::handle_lightbox_key(active, event) {
            return LightboxAction::Handled;
        }
        // Click or Space/Enter on the surface itself toggles
        // play. Conventional video-player behaviour and the
        // primary path for play/pause once focus has landed
        // anywhere inside the lightbox.
        if event.is_click_or_activate(video::KEY_LIGHTBOX_SURFACE) {
            active.toggle_play();
            return LightboxAction::Handled;
        }
        if event.is_click_or_activate(video::KEY_PLAY_PAUSE) {
            active.toggle_play();
            return LightboxAction::Handled;
        }
        if event.is_click_or_activate(video::KEY_MUTE) {
            active.toggle_mute();
            return LightboxAction::Handled;
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
                    let n = damascene_core::widgets::slider::normalized_from_event(rect, x);
                    active.scrubbing = true;
                    active.seek_normalized(n);
                    return LightboxAction::Handled;
                }
                UiEventKind::PointerUp | UiEventKind::Click => {
                    let n = damascene_core::widgets::slider::normalized_from_event(rect, x);
                    active.seek_normalized(n);
                    active.scrubbing = false;
                    return LightboxAction::Handled;
                }
                _ => {}
            }
        }
    }

    // Model lightbox. Open via the inline model preview card or the
    // file-card "View 3D" button; close via Close button, scrim, or
    // Escape. Orbit/pan/zoom inside the panel is handled by
    // damascene's `chart3d` element directly (it consumes its own
    // pointer/wheel events), so there are no per-frame controls here.
    if matches!(event.kind, UiEventKind::Click | UiEventKind::Activate)
        && let Some(route) = event.route()
        && let Some(transfer_id) = model::parse_open_model_key(route)
    {
        return LightboxAction::OpenModel(transfer_id.to_string());
    }
    if lb.model.is_some()
        && (event.is_click_or_activate(model::KEY_LIGHTBOX_CLOSE)
            || (event.is_route(model::KEY_LIGHTBOX_DISMISS) && event.kind == UiEventKind::Click)
            || event.kind == UiEventKind::Escape)
    {
        lb.close_model();
        return LightboxAction::Handled;
    }

    LightboxAction::Ignored
}
