//! Chat sidebar: history rendering, FileOffer attachment cards, and
//! the composer (single-line text input + paperclip + sync button).
//!
//! This module is the aetna port of `rumble-egui`'s chat panel
//! (see `docs/rumble-egui-feature-spec/12-chat-panel.md`) plus the
//! file-offer card / auto-download behaviour that originally only
//! shipped in `rumble-next`.
//!
//! The render side is pure: it consumes `(state, settings, chat_input,
//! selection)` and returns an `El`. The App owns event handling, slash
//! command parsing, and the async file-picker pump.

use std::{
    collections::HashMap,
    sync::LazyLock,
    time::{Duration, Instant},
};

use aetna_core::{prelude::*, surface::SurfaceAlpha};
use rumble_client_traits::file_transfer::{PluginTransferState, TransferStatus};
use rumble_desktop_shell::ChatSettings;
use rumble_protocol::{ChatAttachment, ChatMessage, ChatMessageKind, FileOfferInfo, State, permissions::Permissions};

use crate::{animated_gpu::AnimatedGpu, theme as palette};

/// Decoded image cache, keyed by `transfer_id`. The App maintains this
/// map and passes it through on render so the chat can swap a file
/// card for an inline preview when the underlying transfer is complete.
///
/// Static images carry a single decoded [`Image`]. Animated GIFs carry
/// the full sequence of frames so playback runs without re-decoding.
pub type ImageCache = HashMap<String, CachedImage>;

/// Active file-transfer status keyed by `transfer_id`. Built once per
/// frame from `BackendHandle::transfers()` and threaded through the
/// chat renderer so an in-flight FileOffer card can show progress
/// without each `file_offer_card` call doing its own linear search.
pub type TransferMap = HashMap<String, TransferStatus>;

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
/// moment the GIF wants to advance — no global polling cadence
/// required, and off-screen previews stop driving the loop entirely
/// (visibility filter is layout-rect ∩ viewport).
///
/// Returns `None` for paused playbacks: nothing to redraw for.
fn next_frame_deadline(frames: &[(Image, Duration)], pb: &GifPlayback) -> Option<Duration> {
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
/// independent playhead per message.
#[derive(Clone, Debug)]
pub enum CachedImage {
    Static(Image),
    Animated {
        /// Decoded frames in source order, paired with each frame's
        /// display duration. Always at least 2 entries — single-frame
        /// GIFs collapse to `Static` at decode time.
        frames: Vec<(Image, Duration)>,
    },
}

impl CachedImage {
    /// True when the entry has more than one frame and so warrants
    /// playback state + the play/pause overlay.
    pub fn is_animated(&self) -> bool {
        matches!(self, Self::Animated { .. })
    }

    /// Pick the frame to render. For `Static`, returns the only frame
    /// regardless of `playback`. For `Animated`, returns the frame at
    /// `playback.frame_idx` if supplied, otherwise the first frame.
    pub fn current_frame(&self, playback: Option<&GifPlayback>) -> &Image {
        match self {
            Self::Static(img) => img,
            Self::Animated { frames, .. } => {
                let idx = playback.map(|p| p.frame_idx).unwrap_or(0);
                &frames[idx.min(frames.len() - 1)].0
            }
        }
    }
}

// Player-control SVG icons. Loaded once via `parse_current_color` so
// `.text_color(...)` tints them — these are monochrome glyphs, unlike
// the room-tree status icons (`talking_on`, `muted_self`, etc.) which
// bake their semantic colours into the SVG.
static SVG_PLAY: LazyLock<SvgIcon> =
    LazyLock::new(|| SvgIcon::parse_current_color(include_str!("../assets/icons/play.svg")).expect("play.svg parses"));
static SVG_PAUSE: LazyLock<SvgIcon> = LazyLock::new(|| {
    SvgIcon::parse_current_color(include_str!("../assets/icons/pause.svg")).expect("pause.svg parses")
});
static SVG_CLIPBOARD: LazyLock<SvgIcon> = LazyLock::new(|| {
    SvgIcon::parse_current_color(include_str!("../assets/icons/clipboard.svg")).expect("clipboard.svg parses")
});
static SVG_LIGHTBOX_OPEN: LazyLock<SvgIcon> = LazyLock::new(|| {
    SvgIcon::parse_current_color(include_str!("../assets/icons/lightbox_open.svg")).expect("lightbox_open.svg parses")
});

/// Routed-key constants for the composer's auxiliary buttons. Send-on-
/// Enter is keyed off the input itself (`KEY_INPUT`); the buttons each
/// own their own route.
pub const KEY_INPUT: &str = "chat:input";
pub const KEY_SHARE_FILE: &str = "chat:share-file";
pub const KEY_PASTE_IMAGE: &str = "chat:paste-image";
pub const KEY_SYNC_HISTORY: &str = "chat:sync";

/// Routed keys for the file card context menu.
const KEY_FILE_CTX: &str = "chat:file_ctx";
pub const KEY_FILE_CTX_DISMISS: &str = "chat:file_ctx:dismiss";
pub const KEY_FILE_CTX_SAVE_AS: &str = "chat:file_ctx:save_as";
pub const KEY_FILE_CTX_OPEN_FOLDER: &str = "chat:file_ctx:open_folder";
pub const KEY_FILE_CTX_OPEN: &str = "chat:file_ctx:open";

/// Routed keys for the image lightbox overlay.
pub const KEY_LIGHTBOX_DISMISS: &str = "chat:lightbox:dismiss";
pub const KEY_LIGHTBOX_CLOSE: &str = "chat:lightbox:close";
pub const KEY_LIGHTBOX_ZOOM_IN: &str = "chat:lightbox:zoom-in";
pub const KEY_LIGHTBOX_ZOOM_OUT: &str = "chat:lightbox:zoom-out";
pub const KEY_LIGHTBOX_ZOOM_FIT: &str = "chat:lightbox:zoom-fit";
pub const KEY_LIGHTBOX_IMAGE: &str = "chat:lightbox:image";

/// Zoom range and step used by the lightbox controls. Mirrors the
/// rumble-egui `ImageViewModalState` constants so behaviour ports
/// cleanly: 25%–1000% range, 1.25× per `−` / `+` click, Fit resets.
pub const LIGHTBOX_ZOOM_MIN: f32 = 0.25;
pub const LIGHTBOX_ZOOM_MAX: f32 = 10.0;
pub const LIGHTBOX_ZOOM_STEP: f32 = 1.25;

pub fn download_key(transfer_id: &str) -> String {
    format!("chat:download:{transfer_id}")
}

pub fn preview_key(transfer_id: &str) -> String {
    format!("chat:preview:{transfer_id}")
}

pub fn file_card_key(transfer_id: &str) -> String {
    format!("chat:file-card:{transfer_id}")
}

/// Route for the in-flight transfer's cancel button.
pub fn cancel_key(transfer_id: &str) -> String {
    format!("chat:cancel:{transfer_id}")
}

/// Inverse of [`cancel_key`].
pub fn parse_cancel_key(key: &str) -> Option<&str> {
    key.strip_prefix("chat:cancel:")
}

/// Routed key for the play/pause icon overlaid on an animated preview.
pub fn gif_play_key(transfer_id: &str) -> String {
    format!("chat:gif:play:{transfer_id}")
}

/// Routed key for the explicit "open lightbox" icon overlaid on a
/// preview. Mirrors the click-through behaviour of `preview_key`, but
/// surfaces it as a discoverable affordance alongside play/pause.
pub fn gif_lightbox_key(transfer_id: &str) -> String {
    format!("chat:gif:lightbox:{transfer_id}")
}

pub fn parse_gif_play_key(key: &str) -> Option<&str> {
    key.strip_prefix("chat:gif:play:")
}

pub fn parse_gif_lightbox_key(key: &str) -> Option<&str> {
    key.strip_prefix("chat:gif:lightbox:")
}

/// Parse a `chat:download:<transfer-id>` route back to its transfer id.
pub fn parse_download_key(key: &str) -> Option<&str> {
    key.strip_prefix("chat:download:")
}

/// Parse a `chat:preview:<transfer-id>` route back to its transfer id.
pub fn parse_preview_key(key: &str) -> Option<&str> {
    key.strip_prefix("chat:preview:")
}

pub fn parse_plain_file_card_key(key: &str) -> Option<&str> {
    key.strip_prefix("chat:file-card:")
}

/// Parse a `chat:download:*` or `chat:preview:*` key back to its transfer id.
/// Used by the SecondaryClick handler to detect right-clicks on either card type.
/// The icon-overlay routes (`chat:gif:*`) intentionally don't right-click —
/// they are point actions, not surfaces.
pub fn parse_file_card_key(key: &str) -> Option<&str> {
    parse_plain_file_card_key(key)
        .or_else(|| parse_download_key(key))
        .or_else(|| parse_preview_key(key))
}

/// State for the right-click file context menu.
#[derive(Clone, Debug)]
pub struct FileContextMenu {
    pub transfer_id: String,
    pub name: String,
    /// Local path if the file has been downloaded; `None` while pending.
    pub local_path: Option<std::path::PathBuf>,
    /// Screen-space anchor for the context menu popover.
    pub point: (f32, f32),
}

/// Render the file card context menu as a floating popover overlay.
pub fn render_file_context_menu(menu: &FileContextMenu) -> El {
    let has_file = menu.local_path.is_some();

    let mut save_as = menu_item("Save As…").key(KEY_FILE_CTX_SAVE_AS);
    let mut open_folder = menu_item("Open Containing Folder").key(KEY_FILE_CTX_OPEN_FOLDER);
    let mut open = menu_item("Open").key(KEY_FILE_CTX_OPEN);
    if !has_file {
        save_as = save_as.disabled();
        open_folder = open_folder.disabled();
        open = open.disabled();
    }

    context_menu(
        KEY_FILE_CTX,
        menu.point,
        [
            text(menu.name.clone())
                .semibold()
                .ellipsis()
                .padding(Sides::xy(tokens::SPACE_3, tokens::SPACE_1)),
            divider(),
            open,
            open_folder,
            save_as,
        ],
    )
}

/// Render the chat sidebar (header, history, composer).
#[allow(clippy::too_many_arguments)]
pub fn render(
    state: &State,
    chat_settings: &ChatSettings,
    image_cache: &ImageCache,
    gif_playback: &HashMap<String, GifPlayback>,
    animated_textures: &AnimatedTextureMap,
    video_thumbs: &VideoThumbMap,
    transfers: &TransferMap,
    chat_input: &str,
    selection: &Selection,
    width: f32,
) -> El {
    column([
        text("Chat")
            .title()
            .padding(Sides::xy(tokens::SPACE_4, tokens::SPACE_2)),
        divider(),
        history(
            state,
            chat_settings,
            image_cache,
            gif_playback,
            animated_textures,
            video_thumbs,
            transfers,
        ),
        divider(),
        composer(state, chat_input, selection),
    ])
    .width(Size::Fixed(width))
    .height(Size::Fill(1.0))
    .fill(tokens::CARD)
}

fn history(
    state: &State,
    chat_settings: &ChatSettings,
    image_cache: &ImageCache,
    gif_playback: &HashMap<String, GifPlayback>,
    animated_textures: &AnimatedTextureMap,
    video_thumbs: &VideoThumbMap,
    transfers: &TransferMap,
) -> El {
    if state.chat_messages.is_empty() {
        let placeholder = text(if state.connection.is_connected() {
            "No messages yet"
        } else {
            "Connect to a server to start chatting"
        })
        .muted()
        .wrap_text();
        return scroll([placeholder])
            .padding(Sides::xy(tokens::SPACE_4, tokens::SPACE_2))
            .gap(tokens::SPACE_1)
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0));
    }

    let lines: Vec<El> = state
        .chat_messages
        .iter()
        .map(|msg| {
            render_message(
                msg,
                chat_settings,
                image_cache,
                gif_playback,
                animated_textures,
                video_thumbs,
                transfers,
            )
        })
        .collect();

    scroll(lines)
        .padding(Sides::xy(tokens::SPACE_4, tokens::SPACE_2))
        .gap(tokens::SPACE_1)
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0))
}

fn render_message(
    msg: &ChatMessage,
    chat_settings: &ChatSettings,
    image_cache: &ImageCache,
    gif_playback: &HashMap<String, GifPlayback>,
    animated_textures: &AnimatedTextureMap,
    video_thumbs: &VideoThumbMap,
    transfers: &TransferMap,
) -> El {
    let prefix = if chat_settings.show_timestamps {
        format!("[{}] ", chat_settings.timestamp_format.format(msg.timestamp))
    } else {
        String::new()
    };

    let body_text = if msg.is_local {
        format!("{}{}", prefix, msg.text)
    } else {
        match &msg.kind {
            ChatMessageKind::Room => format!("{}{}: {}", prefix, msg.sender, msg.text),
            ChatMessageKind::DirectMessage { .. } => {
                format!("{}[DM] {}: {}", prefix, msg.sender, msg.text)
            }
            ChatMessageKind::Tree => format!("{}[Tree] {}: {}", prefix, msg.sender, msg.text),
        }
    };

    let mut line = paragraph(body_text).font_size(tokens::TEXT_XS.size);
    line = if msg.is_local {
        line.text_color(palette::CHAT_SYS).italic()
    } else {
        match &msg.kind {
            ChatMessageKind::Room => line,
            ChatMessageKind::DirectMessage { .. } => line.text_color(palette::CHAT_DM),
            ChatMessageKind::Tree => line.text_color(palette::CHAT_TREE),
        }
    };

    // Attachments render below the text line as their own cards. Local
    // messages don't carry attachments, but check `attachment` first so
    // a future variant slots in cleanly. Image previews take priority
    // over video previews when both happen to materialise (the image
    // cache only tracks image extensions today, so this is moot, but
    // matters if the format detection ever overlaps).
    match msg.attachment.as_ref() {
        Some(ChatAttachment::FileOffer(offer)) => {
            let attachment = if let Some(cached) = image_cache.get(&offer.transfer_id) {
                let playback = gif_playback.get(&offer.transfer_id);
                let gpu = animated_textures.get(&offer.transfer_id);
                image_preview(offer, cached, playback, gpu)
            } else if let Some(thumb) = video_thumbs.get(&offer.transfer_id) {
                video_preview(offer, thumb)
            } else {
                file_offer_card(offer, transfers.get(&offer.transfer_id))
            };
            column([line, attachment]).gap(tokens::SPACE_1).width(Size::Fill(1.0))
        }
        None => line,
    }
}

fn file_offer_card(offer: &FileOfferInfo, status: Option<&TransferStatus>) -> El {
    let header = row([
        icon(IconName::FileText).text_color(tokens::MUTED_FOREGROUND),
        text(offer.name.clone()).semibold().ellipsis(),
    ])
    .gap(tokens::SPACE_1)
    .align(Align::Center)
    .width(Size::Fill(1.0));

    let mime = if offer.mime.is_empty() {
        "unknown".to_string()
    } else {
        offer.mime.clone()
    };
    let meta = text(format!("{} · {mime}", format_size(offer.size)))
        .muted()
        .font_size(tokens::TEXT_XS.size);

    // Active transfer (downloading or uploading and not yet finished):
    // show progress + speed + ETA in place of the Download button. If
    // the underlying TransferStatus reports an error, fall back to the
    // standard Download button so the user can retry.
    let body = match status {
        Some(s) if !s.is_finished && s.error.is_none() && is_in_flight(s.state) => transfer_progress_block(s),
        _ => action_row(offer, status),
    };

    column([header, meta, body])
        .key(file_card_key(&offer.transfer_id))
        .gap(tokens::SPACE_1)
        .padding(Sides::all(tokens::SPACE_2))
        .fill(tokens::SECONDARY)
        .stroke(tokens::BORDER)
        .stroke_width(1.0)
        .radius(tokens::RADIUS_MD)
        .width(Size::Fill(1.0))
}

/// True when a transfer is active enough that we should render a
/// progress bar rather than a Download button.
fn is_in_flight(state: PluginTransferState) -> bool {
    matches!(
        state,
        PluginTransferState::Initializing | PluginTransferState::Downloading | PluginTransferState::Paused
    )
}

fn action_row(offer: &FileOfferInfo, status: Option<&TransferStatus>) -> El {
    let mut buttons: Vec<El> = vec![spacer()];

    // Downloaded video files get a Play button that opens the
    // video lightbox. Same look as Download but routes through
    // the video module's open key.
    let is_playable_video = matches!(
        status,
        Some(s) if s.is_finished && s.error.is_none() && s.local_path.is_some(),
    ) && crate::video::is_video_name(&offer.name);
    if is_playable_video {
        buttons.push(
            button_with_icon(IconName::Activity, "Play")
                .key(crate::video::open_video_key(&offer.transfer_id))
                .primary(),
        );
    }

    buttons.push(
        button_with_icon(IconName::Download, "Download")
            .key(download_key(&offer.transfer_id))
            .primary(),
    );

    row(buttons)
        .gap(tokens::SPACE_2)
        .width(Size::Fill(1.0))
        .align(Align::Center)
}

/// Progress bar + bytes-transferred + speed/ETA line for an in-flight
/// transfer. The bar is determinate when we know the file size,
/// indeterminate otherwise (size==0 sentinel).
fn transfer_progress_block(status: &TransferStatus) -> El {
    let bar: El = if status.size == 0 {
        progress_indeterminate(tokens::PRIMARY)
    } else {
        progress(status.progress.clamp(0.0, 1.0), tokens::PRIMARY)
    };

    let speed = if status.state == PluginTransferState::Paused {
        0
    } else if status.download_speed > 0 {
        status.download_speed
    } else {
        status.upload_speed
    };

    let pct = (status.progress.clamp(0.0, 1.0) * 100.0).round() as i32;
    let transferred = (status.size as f32 * status.progress.clamp(0.0, 1.0)) as u64;
    let bytes_label = if status.size > 0 {
        format!("{} / {}", format_size(transferred), format_size(status.size))
    } else {
        format_size(transferred)
    };
    let speed_label = match status.state {
        PluginTransferState::Paused => "Paused".to_string(),
        _ if speed > 0 => format!("{}/s", format_size(speed)),
        _ => "…".to_string(),
    };
    let eta_label = match status.state {
        PluginTransferState::Paused => String::new(),
        _ if speed > 0 && status.size > transferred => {
            let remaining = status.size - transferred;
            format!(" · {} left", format_eta(remaining, speed))
        }
        _ => String::new(),
    };

    let info_text = text(format!("{pct}% · {bytes_label} · {speed_label}{eta_label}"))
        .muted()
        .font_size(tokens::TEXT_XS.size);
    // Cancel sits on the right of the info line so it stays at a
    // predictable place across upload + download cards. The relay
    // backend doesn't support pause/resume yet, so we don't render
    // those buttons — they'd just bail.
    let cancel_btn = icon_button(IconName::X)
        .key(cancel_key(&status.id.0))
        .ghost()
        .tooltip("Cancel");
    let info_line = row([info_text.width(Size::Fill(1.0)), cancel_btn])
        .gap(tokens::SPACE_1)
        .align(Align::Center)
        .width(Size::Fill(1.0));

    column([bar, info_line]).gap(tokens::SPACE_1).width(Size::Fill(1.0))
}

/// Coarse mm:ss / Hh Mm formatter for a remaining-time estimate.
/// Caller guarantees `speed > 0`.
fn format_eta(remaining_bytes: u64, speed_bps: u64) -> String {
    let secs = remaining_bytes / speed_bps.max(1);
    if secs >= 3600 {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// Inline thumbnail card for an image attachment whose underlying
/// transfer is complete. The preview rect is fixed-height, fill-width;
/// `Contain` fit then letterboxes the source so portrait and landscape
/// images both render with the correct aspect ratio without us having
/// to know the parent column's actual width.
///
/// The whole card carries a `chat:preview:<transfer-id>` route so a
/// click anywhere on it opens the lightbox at full size. `focusable`
/// plus the pointer cursor surface the affordance to keyboard and
/// mouse users alike.
///
/// Animated entries get a YouTube-style icon pill in the bottom-right
/// of the preview area: play/pause toggle and an explicit "open
/// lightbox" icon. These nest above the card so their click events
/// route to `chat:gif:*` rather than the underlying preview key.
///
/// Animated entries with a live GPU mirror render through aetna's
/// `surface()` widget — the per-message texture is updated in-place
/// each frame transition (in the App's `before_paint`), so playback
/// doesn't churn aetna's image content-hash cache. Static images, and
/// animated entries before their GPU mirror has been allocated (a
/// one-frame race after decode lands), use the regular `image()`
/// widget with `ImageFit::Contain`.
fn image_preview(
    offer: &FileOfferInfo,
    cached: &CachedImage,
    playback: Option<&GifPlayback>,
    gpu: Option<&AnimatedGpu>,
) -> El {
    const PREVIEW_HEIGHT: f32 = 220.0;

    let preview_layer: El = match (cached, gpu) {
        (CachedImage::Animated { frames, .. }, Some(gpu)) => {
            let is_playing = playback.map(|p| p.playing).unwrap_or(false);
            let deadline = playback.and_then(|pb| next_frame_deadline(frames, pb));
            animated_surface_preview(&offer.transfer_id, gpu, is_playing, deadline, PREVIEW_HEIGHT)
        }
        _ => {
            let frame = cached.current_frame(playback);
            let preview = image(frame.clone())
                .image_fit(ImageFit::Contain)
                .radius(tokens::RADIUS_SM)
                .width(Size::Fill(1.0))
                .height(Size::Fixed(PREVIEW_HEIGHT));
            if cached.is_animated() {
                // Animated cache entry whose GPU mirror hasn't
                // materialized yet (one-frame race after decode).
                // Fall through to image()-based playback for this
                // frame; the next frame swaps to surface().
                let is_playing = playback.map(|p| p.playing).unwrap_or(false);
                stack([preview, gif_controls_overlay(&offer.transfer_id, is_playing)])
                    .width(Size::Fill(1.0))
                    .height(Size::Fixed(PREVIEW_HEIGHT))
            } else {
                preview
            }
        }
    };

    let caption = text(format!("{} · {}", offer.name, format_size(offer.size)))
        .muted()
        .font_size(tokens::TEXT_XS.size)
        .ellipsis();

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

/// Inline preview card for a downloaded video: poster thumbnail
/// (first frame extracted via libmpv) with a centered play
/// overlay, caption beneath. Whole card is clickable —
/// activating it routes through [`crate::video::open_video_key`]
/// the same way the file-card "Play" button does.
fn video_preview(offer: &FileOfferInfo, thumb: &Image) -> El {
    const PREVIEW_HEIGHT: f32 = 220.0;

    let poster = image(thumb.clone())
        .image_fit(ImageFit::Contain)
        .radius(tokens::RADIUS_SM)
        .width(Size::Fill(1.0))
        .height(Size::Fixed(PREVIEW_HEIGHT));

    // Centered play badge: a translucent dark backplate with the
    // existing play SVG centred on top. Sized to feel inviting
    // without dominating the poster — about a third of the card
    // height.
    let play_badge = stack([icon(SVG_PLAY.clone())
        .text_color(tokens::FOREGROUND)
        .width(Size::Fixed(36.0))
        .height(Size::Fixed(36.0))])
    .padding(Sides::all(tokens::SPACE_3))
    .fill(tokens::OVERLAY_SCRIM)
    .radius(tokens::RADIUS_PILL);

    // Center the badge over the poster via leading+trailing
    // spacers in both axes — the same parking trick the GIF
    // controls overlay uses, but for the centre slot rather than
    // a corner.
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

    let preview_layer = stack([poster, centred_badge])
        .width(Size::Fill(1.0))
        .height(Size::Fixed(PREVIEW_HEIGHT));

    let caption = text(format!("{} · {}", offer.name, format_size(offer.size)))
        .muted()
        .font_size(tokens::TEXT_XS.size)
        .ellipsis();

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

/// Build the surface-based preview for an animated entry. Sizes the
/// surface to the texture's natural aspect ratio (height-pinned at
/// `PREVIEW_HEIGHT`, width capped at `PREVIEW_MAX_WIDTH` for very wide
/// images), and centers it horizontally in the card width with
/// leading/trailing spacers — equivalent to a height-only
/// `ImageFit::Contain` with no letterbox bands. The controls overlay
/// stacks above the surface so its bottom-right corner sits on the
/// image, not on empty card padding.
fn animated_surface_preview(
    transfer_id: &str,
    gpu: &AnimatedGpu,
    is_playing: bool,
    next_frame_in: Option<Duration>,
    preview_height: f32,
) -> El {
    /// Cap on the inscribed surface width. Beyond this the preview
    /// would dominate narrow chat panels; very wide images
    /// downsample to fit.
    const PREVIEW_MAX_WIDTH: f32 = 480.0;

    let (tw, th) = gpu.size();
    let aspect = (tw as f32) / (th.max(1) as f32);
    let mut w = preview_height * aspect;
    let mut h = preview_height;
    if w > PREVIEW_MAX_WIDTH {
        w = PREVIEW_MAX_WIDTH;
        h = w / aspect;
    }

    let mut surface_el = surface(gpu.app_texture().clone())
        .surface_alpha(SurfaceAlpha::Straight)
        .width(Size::Fixed(w))
        .height(Size::Fixed(h))
        .radius(tokens::RADIUS_SM);
    if let Some(deadline) = next_frame_in {
        // Aetna's runtime aggregates this with every other visible
        // widget's deadline and schedules a single host wake-up for
        // the tightest one. Off-screen previews drop out of the
        // aggregate automatically and stop driving redraws.
        surface_el = surface_el.redraw_within(deadline);
    }

    let inscribed = stack([surface_el, gif_controls_overlay(transfer_id, is_playing)])
        .width(Size::Fixed(w))
        .height(Size::Fixed(h));

    row([
        spacer().width(Size::Fill(1.0)),
        inscribed,
        spacer().width(Size::Fill(1.0)),
    ])
    .width(Size::Fill(1.0))
    .height(Size::Fixed(h))
}

/// Bottom-right pill of icon buttons overlaid on an animated image
/// preview. Pushed to the corner with leading spacers in both axes so
/// the pill keeps its shape regardless of image aspect ratio. The
/// translucent dark backplate keeps the icons readable against any
/// frame contents.
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

    // While playing the pill hides at rest and fades in on hover of
    // the parent card or the pill itself. While paused the pill stays
    // visible so the play-button affordance is always discoverable.
    if is_playing {
        pill = pill.hover_alpha(0.0, 1.0);
    }

    // Push to bottom-right inside the parent stack: leading vertical
    // and horizontal spacers consume excess space, parking the pill in
    // the corner. Outer padding keeps the pill off the rounded edge of
    // the image rect.
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

/// Returns true if `name`'s extension is one we can decode for inline
/// previews. Mirrors the `image` crate features compiled into the
/// aetna client (png/jpg/jpeg/gif/webp/bmp/ico/tif/tiff).
pub fn is_image_name(name: &str) -> bool {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    matches!(
        ext.as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "tif" | "tiff"
    )
}

fn composer(state: &State, chat_input: &str, selection: &Selection) -> El {
    let connected = state.connection.is_connected();
    let can_chat = Permissions::from_bits_truncate(state.effective_permissions).contains(Permissions::TEXT_MESSAGE);

    let placeholder = if !connected {
        Some("Connect to a server to chat")
    } else if !can_chat {
        Some("You don't have permission to chat in this room")
    } else {
        None
    };

    let opts = match placeholder {
        Some(p) => text_input::TextInputOpts::default().placeholder(p),
        None => text_input::TextInputOpts::default(),
    };
    let mut input = text_input::text_input_with(chat_input, selection, KEY_INPUT, opts).width(Size::Fill(1.0));
    if !connected || !can_chat {
        input = input.disabled();
    }

    let mut share = icon_button(IconName::Upload)
        .key(KEY_SHARE_FILE)
        .ghost()
        .tooltip("Share a file");
    let mut paste = icon_button(SVG_CLIPBOARD.clone())
        .key(KEY_PASTE_IMAGE)
        .ghost()
        .tooltip("Paste image from clipboard");
    let mut sync = icon_button(IconName::RefreshCw)
        .key(KEY_SYNC_HISTORY)
        .ghost()
        .tooltip("Sync chat history");
    if !connected {
        share = share.disabled();
        paste = paste.disabled();
        sync = sync.disabled();
    }

    column([
        row([input])
            .padding(Sides::xy(tokens::SPACE_4, tokens::SPACE_2))
            .width(Size::Fill(1.0)),
        row([share, paste, sync, spacer()])
            .gap(tokens::SPACE_1)
            .padding(Sides {
                left: tokens::SPACE_4,
                right: tokens::SPACE_4,
                top: 0.0,
                bottom: tokens::SPACE_2,
            })
            .width(Size::Fill(1.0))
            .align(Align::Center),
    ])
    .width(Size::Fill(1.0))
}

/// State for the image lightbox. Keyed by `transfer_id` so a re-render
/// looks the image back up out of [`ImageCache`] — that means a transfer
/// being garbage-collected mid-view dismisses the overlay cleanly the
/// next frame instead of leaving a stale clone behind.
#[derive(Clone, Debug)]
pub struct Lightbox {
    pub transfer_id: String,
    pub name: String,
    /// Zoom factor applied to the image's painted rect. `1.0` matches
    /// the Fit-to-frame baseline (image fills the lightbox body via
    /// `ImageFit::Contain` at its natural aspect).
    pub zoom: f32,
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
            pan: (0.0, 0.0),
            drag: PanDrag::default(),
        }
    }

    pub fn fit(&mut self) {
        self.zoom = 1.0;
        self.pan = (0.0, 0.0);
    }

    pub fn zoom_in(&mut self) {
        self.set_zoom(self.zoom * LIGHTBOX_ZOOM_STEP);
    }

    pub fn zoom_out(&mut self) {
        self.set_zoom(self.zoom / LIGHTBOX_ZOOM_STEP);
    }

    pub fn set_zoom(&mut self, zoom: f32) {
        self.zoom = zoom.clamp(LIGHTBOX_ZOOM_MIN, LIGHTBOX_ZOOM_MAX);
        // At zoom 1× the image is anchored centre by `Contain`, so any
        // residual pan would just bias the framing. Reset it for a
        // predictable Fit result whenever we land back at unity.
        if (self.zoom - 1.0).abs() < f32::EPSILON {
            self.pan = (0.0, 0.0);
        }
    }
}

/// Ephemeral pan-drag state: anchor pointer + pan value at
/// `PointerDown`, used by `Drag` to compute the new pan as
/// `start_pan + (current_pointer - anchor_pointer)`.
#[derive(Clone, Debug, Default)]
pub struct PanDrag {
    pub anchor: Option<((f32, f32), (f32, f32))>,
}

/// Build the click-to-enlarge image viewer overlay. The caller picks
/// the most-detailed image available (full-resolution lightbox decode
/// when ready, otherwise the thumbnail from [`ImageCache`]) and
/// passes it through. For animated entries `playback` selects the
/// current frame so the lightbox stays in lockstep with the inline
/// preview's playhead.
pub fn render_lightbox(
    lightbox: &Lightbox,
    cached: &CachedImage,
    playback: Option<&GifPlayback>,
    gpu: Option<&AnimatedGpu>,
) -> El {
    let header = lightbox_header(lightbox);
    let body = lightbox_body(lightbox, cached, playback, gpu);

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
    // gutters on every side. The container is itself transparent —
    // clicks land on the scrim sibling underneath, dismissing the
    // overlay, while clicks on the panel itself are absorbed by
    // `block_pointer()`.
    let centered = stack([panel]).fill_size().padding(Sides::all(tokens::SPACE_7));

    overlay([scrim(KEY_LIGHTBOX_DISMISS), centered])
}

fn lightbox_header(lightbox: &Lightbox) -> El {
    let zoom_pct = (lightbox.zoom * 100.0).round() as i32;

    let mut zoom_out = button("−").key(KEY_LIGHTBOX_ZOOM_OUT).ghost();
    if lightbox.zoom <= LIGHTBOX_ZOOM_MIN + f32::EPSILON {
        zoom_out = zoom_out.disabled();
    }
    let mut zoom_in = button("+").key(KEY_LIGHTBOX_ZOOM_IN).ghost();
    if lightbox.zoom >= LIGHTBOX_ZOOM_MAX - f32::EPSILON {
        zoom_in = zoom_in.disabled();
    }
    let mut fit = button("Fit").key(KEY_LIGHTBOX_ZOOM_FIT).ghost();
    if (lightbox.zoom - 1.0).abs() < f32::EPSILON && lightbox.pan == (0.0, 0.0) {
        fit = fit.disabled();
    }

    row([
        text(lightbox.name.clone()).title().ellipsis(),
        spacer(),
        zoom_out,
        text(format!("{zoom_pct}%"))
            .muted()
            .font_size(tokens::TEXT_SM.size)
            .width(Size::Fixed(56.0))
            .text_align(TextAlign::Center),
        zoom_in,
        fit,
        button("Close").key(KEY_LIGHTBOX_CLOSE),
    ])
    .gap(tokens::SPACE_2)
    .align(Align::Center)
    .width(Size::Fill(1.0))
}

fn lightbox_body(
    lightbox: &Lightbox,
    cached: &CachedImage,
    playback: Option<&GifPlayback>,
    gpu: Option<&AnimatedGpu>,
) -> El {
    // Inner element: `Contain` fit at base size; `.scale(zoom)` zooms
    // around its centre, `.translate(pan)` then offsets the result.
    // Both transforms are paint-time so layout stays stable as the
    // user drags / zooms — no re-layout cost per frame, and the
    // body's allocated rect stays the lightbox body so subsequent
    // pointer events still target the same region.
    //
    // For animated entries with a live GPU mirror we re-use the same
    // `surface()` texture the inline preview is already updating in
    // `before_paint`, with `surface_fit(Contain)` doing the
    // letterbox into the dynamic Fill×Fill body rect. Avoids a
    // second copy through aetna's image content-hash cache and keeps
    // the lightbox in lockstep with the inline playhead at zero
    // additional GPU cost. Static images and the one-frame race
    // before the GPU mirror is allocated fall through to `image()`.
    let inner = match (cached, gpu) {
        (CachedImage::Animated { frames, .. }, Some(gpu)) => {
            let mut el = surface(gpu.app_texture().clone())
                .surface_alpha(SurfaceAlpha::Straight)
                .surface_fit(ImageFit::Contain)
                .radius(tokens::RADIUS_MD)
                .width(Size::Fill(1.0))
                .height(Size::Fill(1.0))
                .scale(lightbox.zoom)
                .translate(lightbox.pan.0, lightbox.pan.1);
            if let Some(deadline) = playback.and_then(|pb| next_frame_deadline(frames, pb)) {
                el = el.redraw_within(deadline);
            }
            el
        }
        _ => {
            let img = cached.current_frame(playback);
            image(img.clone())
                .image_fit(ImageFit::Contain)
                .radius(tokens::RADIUS_MD)
                .width(Size::Fill(1.0))
                .height(Size::Fill(1.0))
                .scale(lightbox.zoom)
                .translate(lightbox.pan.0, lightbox.pan.1)
        }
    };

    // The body is the drag surface. `clip()` so the panned/zoomed
    // image doesn't bleed past the panel; `.key(...)` captures the
    // pointer events so the App's drag handler can update `pan`.
    stack([inner])
        .key(KEY_LIGHTBOX_IMAGE)
        .clip()
        .fill(tokens::CARD)
        .radius(tokens::RADIUS_MD)
        .cursor(if lightbox.drag.anchor.is_some() {
            Cursor::Grabbing
        } else if lightbox.zoom > 1.0 {
            Cursor::Grab
        } else {
            Cursor::Default
        })
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0))
}

/// Format a byte size as a human-readable string (B / KB / MB / GB).
/// Mirrors the egui client's `format_size` for visual parity.
fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}
