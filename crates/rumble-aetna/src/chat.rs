//! Chat sidebar: history rendering, FileOffer attachment cards, and
//! the composer (multi-line text area + paperclip + sync button).
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
    sync::{Arc, LazyLock, Mutex},
    time::{Duration, Instant},
};

use aetna_core::{prelude::*, surface::SurfaceAlpha};
use aetna_markdown::md;
use linkify::{LinkFinder, LinkKind};
use rumble_client::{ChatMessage, ChatMessageKind, State};
use rumble_client_traits::file_transfer::{TransferDirection, TransferStage, TransferStatus};
use rumble_desktop_shell::ChatSettings;
use rumble_protocol::{permissions::Permissions, proto::RelayFileSharePayload};

use crate::{animated_gpu::AnimatedGpu, media_cache::MediaCache, theme as palette};

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
/// independent playhead per message; `Svg` is a parsed vector asset
/// rendered natively by aetna at any size (no rasterization, crisp
/// at any zoom in the lightbox).
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
    /// True when the entry has more than one frame and so warrants
    /// playback state + the play/pause overlay.
    pub fn is_animated(&self) -> bool {
        matches!(self, Self::Animated { .. })
    }

    /// True when the entry is a vector asset rendered via aetna's
    /// vector path rather than a raster `image()`.
    pub fn is_vector(&self) -> bool {
        matches!(self, Self::Svg(_))
    }

    /// Pick the raster frame to render. Returns `None` for vector
    /// entries since they have no decoded raster representation —
    /// callers must branch on [`Self::is_vector`] first and emit
    /// `vector(..)` instead of `image(..)`.
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

    /// Natural dimensions of the entry. Raster entries report the
    /// frame's pixel size; vector entries report the SVG view-box
    /// extents rounded to the nearest integer.
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
pub const KEY_LIGHTBOX_ZOOM_NATURAL: &str = "chat:lightbox:zoom-natural";
pub const KEY_LIGHTBOX_IMAGE: &str = "chat:lightbox:image";

/// Zoom range and step used by the lightbox controls. The numeric zoom
/// is relative to decoded image pixels: `1.0` paints one image pixel as
/// one logical pixel. Fit-to-window is a separate mode because its
/// actual scale depends on the lightbox body's runtime dimensions.
pub const LIGHTBOX_ZOOM_MIN: f32 = 0.25;
pub const LIGHTBOX_ZOOM_MAX: f32 = 10.0;
pub const LIGHTBOX_ZOOM_STEP: f32 = 0.20;

/// Decode a relay-plugin file-share payload off a chat message's
/// attachment, if the attachment exists and belongs to the relay plugin.
/// Returns `None` for messages with no attachment or attachments from
/// other plugins.
pub fn relay_payload(msg: &ChatMessage) -> Option<RelayFileSharePayload> {
    let att = msg.attachment.as_ref()?;
    if att.namespace != rumble_desktop::FILE_TRANSFER_RELAY_NAMESPACE {
        return None;
    }
    rumble_desktop::decode_relay_payload(&att.payload)
}

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

/// Route for the inline "Open" button on a completed sender/receiver card.
pub fn open_key(transfer_id: &str) -> String {
    format!("chat:open:{transfer_id}")
}

/// Inverse of [`open_key`].
pub fn parse_open_key(key: &str) -> Option<&str> {
    key.strip_prefix("chat:open:")
}

/// Route for the inline "Reveal in folder" button on a completed card.
pub fn reveal_key(transfer_id: &str) -> String {
    format!("chat:reveal:{transfer_id}")
}

/// Inverse of [`reveal_key`].
pub fn parse_reveal_key(key: &str) -> Option<&str> {
    key.strip_prefix("chat:reveal:")
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
    media_cache: &MediaCache,
    transfers: &TransferMap,
    pending_cancel_confirm: &HashMap<String, Instant>,
    chat_input: &str,
    selection: &Selection,
    my_user_id: Option<u64>,
    own_username: &str,
) -> El {
    column([
        text("Chat")
            .title()
            .padding(Sides::xy(tokens::SPACE_4, tokens::SPACE_2)),
        divider(),
        history(
            state,
            chat_settings,
            media_cache,
            transfers,
            pending_cancel_confirm,
            my_user_id,
            own_username,
        ),
        divider(),
        composer(state, chat_input, selection),
    ])
    .height(Size::Fill(1.0))
    .fill(tokens::CARD)
}

fn history(
    state: &State,
    chat_settings: &ChatSettings,
    media_cache: &MediaCache,
    transfers: &TransferMap,
    pending_cancel_confirm: &HashMap<String, Instant>,
    my_user_id: Option<u64>,
    own_username: &str,
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
            .height(Size::Fill(1.0))
            .pin_end();
    }

    let lines: Vec<El> = state
        .chat_messages
        .iter()
        // SenderMirror entries are sender-side history twins of broadcasts
        // whose SenderDraft card is already on screen — never render.
        .filter(|msg| msg.visibility.renders_locally())
        .map(|msg| {
            render_message(
                msg,
                chat_settings,
                media_cache,
                transfers,
                pending_cancel_confirm,
                my_user_id,
                own_username,
            )
        })
        .collect();

    scroll(lines)
        .padding(Sides::xy(tokens::SPACE_4, tokens::SPACE_2))
        .gap(tokens::SPACE_1)
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0))
        .pin_end()
}

fn render_message(
    msg: &ChatMessage,
    chat_settings: &ChatSettings,
    media_cache: &MediaCache,
    transfers: &TransferMap,
    pending_cancel_confirm: &HashMap<String, Instant>,
    my_user_id: Option<u64>,
    own_username: &str,
) -> El {
    let msg_key = u128::from_le_bytes(msg.id);
    // Prefer server-assigned `sender_id` for identity matching — usernames
    // are client-supplied, not unique (two clients on one machine share
    // `$USER` by default), and would mis-classify the receiver as the
    // sender. Fall back to username only when `sender_id` is absent (older
    // peers in chat-history sync, legacy servers).
    let is_own = if msg.visibility.is_system() {
        false
    } else if let (Some(mine), Some(theirs)) = (my_user_id, msg.sender_id) {
        mine == theirs
    } else {
        msg.sender_id.is_none() && msg.sender == own_username
    };
    let prefix = if chat_settings.show_timestamps {
        format!("[{}] ", chat_settings.timestamp_format.format(msg.timestamp))
    } else {
        String::new()
    };

    // System notices with no attachment: plain italic system text.
    // System entries WITH an attachment (rare in practice) fall through
    // to the normal render path so the card displays.
    if msg.visibility.is_system() && msg.attachment.is_none() {
        return paragraph(format!("{prefix}{}", msg.text))
            .font_size(tokens::TEXT_XS.size)
            .text_color(palette::CHAT_SYS)
            .italic()
            .key(format!("chat:msg:{msg_key}:sys"))
            .selectable();
    }

    // Sender header: "{prefix}SenderName:" in semibold, selectable.
    let (header_text, header_color) = match &msg.kind {
        ChatMessageKind::Room => (format!("{prefix}{}:", msg.sender), None),
        ChatMessageKind::DirectMessage { .. } => (format!("{prefix}[DM] {}:", msg.sender), Some(palette::CHAT_DM)),
        ChatMessageKind::Tree => (format!("{prefix}[Tree] {}:", msg.sender), Some(palette::CHAT_TREE)),
    };
    let mut header = text(header_text)
        .semibold()
        .wrap_text()
        .font_size(tokens::TEXT_XS.size)
        .key(format!("chat:msg:{msg_key}:hdr"))
        .selectable();
    if let Some(color) = header_color {
        header = header.text_color(color);
    }

    // Body. Messages that use markdown syntax go through the markdown
    // renderer (which already produces clickable `[label](url)` links).
    // Everything else takes a plain-text path that linkifies bare URLs
    // into the same `text_link` runs aetna-markdown emits, so the
    // user-visible behavior is consistent without paying the
    // false-positive cost of running pulldown-cmark over casual chat
    // text (stray `*` italicizing, etc.).
    let body = if looks_like_markdown(&msg.text) {
        md(&msg.text).width(Size::Fill(1.0))
    } else {
        plain_with_links(&msg.text, msg_key).width(Size::Fill(1.0))
    };

    // Attachment dispatch: classify into an explicit AttachmentView,
    // then render the chosen variant. The view enum makes precedence
    // a data fact (Failed beats Image beats Video beats File) so the
    // renderer can't accidentally hide a failed share behind a cached
    // thumbnail.
    let attachment: Option<El> = msg.attachment.as_ref().map(|att| {
        let view = attachment_view(att, media_cache, transfers);
        render_attachment_view(view, is_own, pending_cancel_confirm)
    });

    let mut parts: Vec<El> = vec![header, body];
    if let Some(att) = attachment {
        parts.push(att);
    }
    let content = column(parts).gap(tokens::SPACE_1).width(Size::Fill(1.0));

    // Own messages: subtle left accent stripe for visual differentiation.
    if is_own {
        let accent_stripe = spacer()
            .width(Size::Fixed(2.0))
            .height(Size::Fill(1.0))
            .fill(tokens::PRIMARY);
        row([
            accent_stripe,
            content.padding(Sides {
                left: tokens::SPACE_2,
                right: 0.0,
                top: 0.0,
                bottom: 0.0,
            }),
        ])
        .gap(0.0)
        .width(Size::Fill(1.0))
        .align(Align::Stretch)
    } else {
        content
    }
}

/// Classified view of a chat attachment, computed once per message
/// render before any El is built. The variants encode the dispatch
/// precedence as data — `Failed` always beats every preview variant,
/// `Image` beats `Video` (the same file could in principle decode as
/// both), `Video` beats the plain `File` card.
///
/// Lifting this out of an `if let` chain serves two purposes:
///   1. It makes the precedence audit-able. The bug that motivated
///      this refactor was a renderer that picked Video over File-card
///      whenever a thumbnail had decoded — including for an upload
///      that had failed. Now `Failed` lives in its own arm and can't
///      be shadowed.
///   2. It localises the borrow lifetimes. The renderer arms take
///      already-resolved references rather than re-doing the lookups.
enum AttachmentView<'a> {
    /// Inline image preview. The transfer is `Done` and the bytes
    /// have decoded into the chat image cache.
    Image {
        offer: RelayFileSharePayload,
        cached: &'a CachedImage,
        playback: Option<&'a GifPlayback>,
        gpu: Option<&'a AnimatedGpu>,
    },
    /// Inline video poster card. The transfer is `Done` and the first
    /// frame has been extracted into the video-thumb cache.
    Video {
        offer: RelayFileSharePayload,
        thumb: &'a Image,
    },
    /// Plain file card. Status is `None` (history-sync message from
    /// before this session started a transfer for this offer) or one
    /// of `Active`/`Paused`/`Done` (the latter for sender-side or
    /// non-previewable receiver-side completions).
    File {
        offer: RelayFileSharePayload,
        status: Option<&'a TransferStatus>,
    },
    /// Terminal failure. Renders a dedicated error card; takes
    /// precedence over every preview variant so the error always
    /// surfaces.
    Failed {
        offer: RelayFileSharePayload,
        reason: String,
    },
    /// Attachment whose plugin namespace we don't recognise. Falls
    /// back to the human-readable summary the producing plugin
    /// embedded for exactly this case.
    UnknownPlugin { text: String },
}

/// Classify a `ChatAttachment` into the [`AttachmentView`] variant
/// that should be rendered. Pure function — no rendering, just data.
fn attachment_view<'a>(
    att: &rumble_protocol::ChatAttachment,
    media_cache: &'a MediaCache,
    transfers: &'a TransferMap,
) -> AttachmentView<'a> {
    if att.namespace != rumble_desktop::FILE_TRANSFER_RELAY_NAMESPACE {
        return AttachmentView::UnknownPlugin {
            text: att.fallback_text.clone(),
        };
    }
    let Some(offer) = rumble_desktop::decode_relay_payload(&att.payload) else {
        return AttachmentView::UnknownPlugin {
            text: att.fallback_text.clone(),
        };
    };

    let status = transfers.get(&offer.transfer_id);

    // Failure wins over every preview. A failed upload still has its
    // source file on the sender's disk, so the image/video thumb
    // caches might well have populated against it — we deliberately
    // ignore those and surface the error.
    if let Some(s) = status
        && let TransferStage::Failed { reason } = &s.stage
    {
        return AttachmentView::Failed {
            offer,
            reason: reason.clone(),
        };
    }

    if let Some(cached) = media_cache.image_for(&offer.transfer_id) {
        return AttachmentView::Image {
            cached,
            playback: media_cache.gif_playback_for(&offer.transfer_id),
            gpu: media_cache.animated_gpu_for(&offer.transfer_id),
            offer,
        };
    }
    if let Some(thumb) = media_cache.video_thumb_for(&offer.transfer_id) {
        return AttachmentView::Video { offer, thumb };
    }
    AttachmentView::File { offer, status }
}

/// Render the chosen [`AttachmentView`] variant.
fn render_attachment_view(
    view: AttachmentView<'_>,
    is_local_sender: bool,
    pending_cancel_confirm: &HashMap<String, Instant>,
) -> El {
    match view {
        AttachmentView::Image {
            offer,
            cached,
            playback,
            gpu,
        } => image_preview(&offer, cached, playback, gpu),
        AttachmentView::Video { offer, thumb } => video_preview(&offer, thumb),
        AttachmentView::File { offer, status } => {
            file_offer_card(&offer, status, is_local_sender, pending_cancel_confirm)
        }
        AttachmentView::Failed { offer, reason } => failed_card(&offer, &reason, is_local_sender),
        AttachmentView::UnknownPlugin { text } => paragraph(text)
            .muted()
            .italic()
            .font_size(tokens::TEXT_XS.size)
            .selectable()
            .width(Size::Fill(1.0)),
    }
}

fn file_offer_card(
    offer: &RelayFileSharePayload,
    status: Option<&TransferStatus>,
    is_local_sender: bool,
    pending_cancel_confirm: &HashMap<String, Instant>,
) -> El {
    let header = file_card_header(offer);
    let meta = file_card_meta(offer);
    let body = file_card_body(offer, status, is_local_sender, pending_cancel_confirm);

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

/// Header row: file-type icon + the filename, ellipsis-truncated with
/// the full name in a tooltip. Shared by the regular `file_offer_card`
/// and the `failed_card` so a failed share still shows what was
/// attempted.
fn file_card_header(offer: &RelayFileSharePayload) -> El {
    row([
        icon(IconName::FileText).text_color(tokens::MUTED_FOREGROUND),
        text(offer.name.clone())
            .semibold()
            .ellipsis()
            .key(format!("chat:file-card:{}.name", offer.transfer_id))
            .tooltip(offer.name.clone()),
    ])
    .gap(tokens::SPACE_1)
    .align(Align::Center)
    .width(Size::Fill(1.0))
}

/// Secondary line: human size + MIME type. Used by the same two card
/// builders as [`file_card_header`].
fn file_card_meta(offer: &RelayFileSharePayload) -> El {
    let mime = if offer.mime.is_empty() {
        "unknown".to_string()
    } else {
        offer.mime.clone()
    };
    text(format!("{} · {mime}", format_size(offer.size)))
        .muted()
        .font_size(tokens::TEXT_XS.size)
}

/// Determine the action/progress body for a file card based on the
/// user's relationship to the file (sender vs receiver) and the
/// transfer's lifecycle stage. The Failed stage is handled upstream
/// in [`AttachmentView`] — by the time we get here, `status.stage` is
/// `Active`, `Paused`, `Done`, or `status` is `None`.
fn file_card_body(
    offer: &RelayFileSharePayload,
    status: Option<&TransferStatus>,
    is_local_sender: bool,
    pending_cancel_confirm: &HashMap<String, Instant>,
) -> El {
    match status.map(|s| &s.stage) {
        Some(TransferStage::Active { .. } | TransferStage::Paused { .. }) => {
            let s = status.expect("matched on status above");
            let confirm_pending = pending_cancel_confirm.contains_key(&s.id.0);
            transfer_progress_block(s, confirm_pending)
        }
        Some(TransferStage::Done { .. }) if is_local_sender => sender_complete_row(&offer.transfer_id),
        Some(TransferStage::Done { .. }) => receiver_complete_row(offer, status.expect("matched on status above")),
        // No status: sender side renders nothing actionable (their
        // local-only card is just the header). Receiver side gets a
        // Download button to start the fetch.
        None if is_local_sender => row([spacer()]).width(Size::Fill(1.0)),
        None => row([
            spacer(),
            button_with_icon(IconName::Download, "Download")
                .key(download_key(&offer.transfer_id))
                .primary(),
        ])
        .gap(tokens::SPACE_2)
        .width(Size::Fill(1.0))
        .align(Align::Center),
        // Failed is lifted to the AttachmentView::Failed dispatch arm
        // and never reaches the file_card_body; emit a no-op as a
        // defensive default rather than asserting unreachable.
        Some(TransferStage::Failed { .. }) => row([spacer()]).width(Size::Fill(1.0)),
    }
}

/// Failure card body. Replaces the file card entirely when an upload
/// or download fails — the renderer reaches this path via
/// [`AttachmentView::Failed`] regardless of any preview cache hit, so
/// a failed share never has its error hidden behind a stale thumbnail.
fn failed_card(offer: &RelayFileSharePayload, reason: &str, is_local_sender: bool) -> El {
    let label = if is_local_sender {
        "Upload failed"
    } else {
        "Download failed"
    };
    let header = row([text(label)
        .text_color(palette::MUTED_SERVER)
        .font_size(tokens::TEXT_XS.size)
        .semibold()
        .width(Size::Fill(1.0))])
    .width(Size::Fill(1.0));
    let reason_el = paragraph(reason.to_string())
        .text_color(palette::MUTED_SERVER)
        .font_size(tokens::TEXT_XS.size)
        .key(format!("chat:file-card:{}.err", offer.transfer_id))
        .selectable()
        .width(Size::Fill(1.0));

    let card_header = file_card_header(offer);
    let meta = file_card_meta(offer);
    let mut body: Vec<El> = vec![card_header, meta, header, reason_el];
    if is_local_sender {
        body.push(
            text("Retry by sharing the file again")
                .muted()
                .font_size(tokens::TEXT_XS.size),
        );
    }

    column(body)
        .key(file_card_key(&offer.transfer_id))
        .gap(tokens::SPACE_1)
        .padding(Sides::all(tokens::SPACE_2))
        .fill(tokens::SECONDARY)
        .stroke(tokens::BORDER)
        .stroke_width(1.0)
        .radius(tokens::RADIUS_MD)
        .width(Size::Fill(1.0))
}

/// Open + Reveal buttons for a sender whose upload completed.
fn sender_complete_row(transfer_id: &str) -> El {
    row([
        spacer(),
        button_with_icon(IconName::Folder, "Reveal")
            .key(reveal_key(transfer_id))
            .ghost(),
        button_with_icon(IconName::FileText, "Open")
            .key(open_key(transfer_id))
            .primary(),
    ])
    .gap(tokens::SPACE_2)
    .width(Size::Fill(1.0))
    .align(Align::Center)
}

/// Open + Reveal buttons for a receiver whose download completed.
fn receiver_complete_row(offer: &RelayFileSharePayload, status: &TransferStatus) -> El {
    let is_playable_video = status.done_path().is_some() && crate::video::is_video_name(&offer.name);
    let mut btns: Vec<El> = vec![spacer()];
    if is_playable_video {
        btns.push(
            button_with_icon(IconName::Activity, "Play")
                .key(crate::video::open_video_key(&offer.transfer_id))
                .primary(),
        );
    }
    btns.push(
        button_with_icon(IconName::Folder, "Reveal")
            .key(reveal_key(&offer.transfer_id))
            .ghost(),
    );
    btns.push(
        button_with_icon(IconName::FileText, "Open")
            .key(open_key(&offer.transfer_id))
            .primary(),
    );
    row(btns)
        .gap(tokens::SPACE_2)
        .width(Size::Fill(1.0))
        .align(Align::Center)
}

/// Progress bar + bytes-transferred + speed/ETA line for an in-flight
/// transfer. Shows "Uploading" or "Downloading" prefix based on direction.
/// Zero-byte files show a full progress bar rather than indeterminate.
/// `confirm_pending` is true when the first cancel click has been received
/// and the button should show "Cancel?" to prompt confirmation.
fn transfer_progress_block(status: &TransferStatus, confirm_pending: bool) -> El {
    // Stage-extracted progress + speed. `Done`/`Failed` shouldn't reach
    // this function — callers gate on Active/Paused — but the
    // `TransferStage::progress` helper degrades gracefully if they do.
    let progress_frac = status.stage.progress();
    let (speed_bps, paused) = match status.stage {
        TransferStage::Active { speed_bps, .. } => (speed_bps, false),
        TransferStage::Paused { .. } => (0, true),
        _ => (0, false),
    };

    // Zero-byte files: show a complete bar rather than indeterminate progress.
    let bar: El = if status.size == 0 {
        progress(1.0, tokens::PRIMARY)
    } else {
        progress(progress_frac, tokens::PRIMARY)
    };

    let direction_label = match status.direction {
        TransferDirection::Upload => "Uploading",
        TransferDirection::Download => "Downloading",
    };

    let info_text: El = if status.size == 0 {
        text(format!("{direction_label} · 0 B (empty file)"))
            .muted()
            .font_size(tokens::TEXT_XS.size)
    } else {
        let pct = (progress_frac * 100.0).round() as i32;
        let transferred = (status.size as f32 * progress_frac) as u64;
        let bytes_label = format!("{} / {}", format_size(transferred), format_size(status.size));
        let speed_label = if paused {
            "Paused".to_string()
        } else if speed_bps > 0 {
            format!("{}/s", format_size(speed_bps))
        } else {
            "…".to_string()
        };
        let eta_label = if !paused && speed_bps > 0 && status.size > transferred {
            let remaining = status.size - transferred;
            format!(" · {} left", format_eta(remaining, speed_bps))
        } else {
            String::new()
        };

        text(format!(
            "{direction_label} · {pct}% · {bytes_label} · {speed_label}{eta_label}"
        ))
        .muted()
        .font_size(tokens::TEXT_XS.size)
    };

    // Cancel sits on the right of the info line so it stays at a
    // predictable place across upload + download cards. The relay
    // backend doesn't support pause/resume yet, so we don't render
    // those buttons — they'd just bail.
    // Two-click cancel: first click shows "Cancel?" in red; second fires the command.
    let cancel_btn: El = if confirm_pending {
        button("Cancel?")
            .key(cancel_key(&status.id.0))
            .destructive()
            .font_size(tokens::TEXT_XS.size)
    } else {
        icon_button(IconName::X)
            .key(cancel_key(&status.id.0))
            .ghost()
            .tooltip("Cancel")
    };
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

/// Wrap an inner El (`image()`, `surface()`, or `stack([poster, badge])`)
/// in the nested-`Size::Aspect` Contain layout the three preview cards
/// (image, video poster, animated GIF) all share. Centers a
/// natural-aspect rect inside a fill-width column, capped to
/// `max_height` (and optionally `max_width`). The inner El's own
/// `Size::Fill(1.0)` width+height pick up the inscribed rect.
///
/// `vector()` (SVG) and `surface()` (GPU texture) have no built-in
/// Contain analog like `image_fit(ImageFit::Contain)`, so this pattern
/// is how we get aspect-correct letterboxing for them. Static images
/// don't strictly need it (their fit could be done via `image_fit`),
/// but using the same pattern keeps inline overlay positioning
/// consistent — corner badges park on the *visible* rect, not the
/// bounding rect's letterbox bands.
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
                // letterboxing on its own — no need for the nested
                // helper here. Aspect on the height locks the card to
                // the frame's ratio, max_height caps portraits.
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

/// SVG preview layer for inline cards. `vector()` has no built-in
/// Contain (no `vector_fit` analog to `image_fit`/`surface_fit`), so
/// the [`preview_card`] helper is what gives it aspect-correct
/// letterboxing inside a height-capped rect.
fn svg_contain_preview(icon: SvgIcon, height: f32) -> El {
    let [_, _, vw, vh] = icon.vector_asset().view_box;
    let nat_w = vw.round().max(1.0) as u32;
    let nat_h = vh.round().max(1.0) as u32;
    let asset = icon.vector_asset().clone();

    let inner = vector(asset).height(Size::Fill(1.0)).width(Size::Fill(1.0));
    preview_card(inner, nat_w, nat_h, height, None).radius(tokens::RADIUS_SM)
}

/// Fit-to-window SVG variant for the lightbox: fills the available
/// rect on both axes, contain-fits the view-box inside it at layout
/// time.
///
/// Unlike [`svg_contain_preview`] we *can't* swap the layout override
/// for the nested-`Size::Aspect` pattern here: that pattern needs a
/// static cap on at least one axis to anchor the outer rect, and the
/// lightbox body's actual size is only known at layout time. With
/// `Fill × Fill` parent dimensions and no cap, a `Fill × Aspect`
/// outer derives its height from the parent's full width, which
/// overflows when the SVG is wider than the body. So the layout
/// override stays — it has access to the runtime container rect and
/// can do a true min-of-two contain.
fn svg_contain_fill(icon: SvgIcon) -> El {
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
/// (first frame extracted via libmpv) with a centered play
/// overlay, caption beneath. Whole card is clickable —
/// activating it routes through [`crate::video::open_video_key`]
/// the same way the file-card "Play" button does.
fn video_preview(offer: &RelayFileSharePayload, thumb: &Image) -> El {
    const PREVIEW_HEIGHT: f32 = 400.0;

    // The poster card hugs the thumbnail's aspect ratio (via
    // [`preview_card`]), capped at PREVIEW_HEIGHT. The play badge sits
    // inside the same aspect-locked rect so it parks on the *visible*
    // image, not on letterbox bands when the height cap engages.
    let poster = image(thumb.clone())
        .radius(tokens::RADIUS_SM)
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0));

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

/// Build the surface-based preview for an animated entry. Sizes the
/// surface to the texture's natural aspect ratio (height-pinned at
/// `preview_height`, width capped at `PREVIEW_MAX_WIDTH` for very wide
/// images). The controls overlay stacks above the surface inside the
/// same aspect-locked rect so its bottom-right corner sits on the
/// image, not on letterbox padding.
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

    let mut surface_el = surface(gpu.app_texture().clone())
        .surface_alpha(SurfaceAlpha::Straight)
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0))
        .radius(tokens::RADIUS_SM);
    if let Some(deadline) = next_frame_in {
        // Aetna's runtime aggregates this with every other visible
        // widget's deadline and schedules a single host wake-up for
        // the tightest one. Off-screen previews drop out of the
        // aggregate automatically and stop driving redraws.
        surface_el = surface_el.redraw_within(deadline);
    }

    let inner = stack([surface_el, gif_controls_overlay(transfer_id, is_playing)])
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0));
    preview_card(inner, tw, th, preview_height, Some(PREVIEW_MAX_WIDTH))
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
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "tif" | "tiff" | "svg"
    )
}

/// True when the file extension marks an SVG. The decoder branches on
/// this so it can take the vector-native path instead of the raster
/// `image` crate.
pub fn is_svg_name(name: &str) -> bool {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    ext == "svg"
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
        Some(p) => text_area::TextAreaOpts::default().placeholder(p),
        None => text_area::TextAreaOpts::default(),
    };
    let mut input = text_area::text_area_with(chat_input, selection, KEY_INPUT, opts).width(Size::Fill(1.0));
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
    /// in either axis — i.e. there's content outside the viewport that
    /// the user might want to bring into view. Drives both pan-drag
    /// enablement and the grab cursor: when the whole image fits, there
    /// is nothing to pan to.
    ///
    /// Always false in Fit mode (Contain guarantees the image fits) and
    /// when the body hasn't been laid out yet (`body == None`, only on
    /// the very first frame after open).
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

fn fit_step_zoom(
    body_size: Option<(f32, f32)>,
    image_size: Option<(u32, u32)>,
    direction: ZoomDirection,
) -> f32 {
    fit_scale(body_size, image_size)
        .map(|scale| next_zoom_step(scale, direction))
        .unwrap_or_else(|| next_zoom_step(1.0, direction))
}

#[derive(Clone, Copy, Debug)]
pub enum ZoomDirection {
    In,
    Out,
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
    body_size_sink: Arc<Mutex<Option<(f32, f32)>>>,
    cached: &CachedImage,
    playback: Option<&GifPlayback>,
    gpu: Option<&AnimatedGpu>,
) -> El {
    let header = lightbox_header(lightbox);
    let body = lightbox_body(lightbox, body_size_sink, cached, playback, gpu);

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
    body_size_sink: Arc<Mutex<Option<(f32, f32)>>>,
    cached: &CachedImage,
    playback: Option<&GifPlayback>,
    gpu: Option<&AnimatedGpu>,
) -> El {
    let (natural_w, natural_h) = {
        let (w, h) = cached.current_frame_size(playback);
        (w as f32, h as f32)
    };
    let fit_to_window = lightbox.fit_to_window;
    // Last frame's laid-out body size, used to decide the grab cursor.
    // One frame stale (the closure below writes this frame's size); a
    // cursor flicker on the very first frame after resize is harmless.
    let prev_body_size = body_size_sink.lock().ok().and_then(|s| *s);

    // In Fit mode the image/surface fills the body and `Contain`
    // computes the runtime scale from the available window. In explicit
    // zoom mode the element's layout rect is the decoded image's natural
    // pixel size and `.scale(zoom)` paints relative to that, so `100%`
    // means one source pixel per logical pixel.
    //
    // For animated entries with a live GPU mirror we re-use the same
    // `surface()` texture the inline preview is already updating in
    // `before_paint`. Static images and the one-frame race before the
    // GPU mirror is allocated fall through to `image()`.
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
            // Vector path: aetna re-tessellates per frame so zoom is
            // crisp at any scale. There's no `vector_fit`, so the Fit
            // branch uses a layout-time contain-fit; the explicit-zoom
            // branch fixes the rect to the natural view-box size and
            // lets the El's scale/translate transform paint at the
            // requested zoom — mirrors the raster `image` path.
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

    // The body is the drag surface. `clip()` so the panned/zoomed
    // image doesn't bleed past the panel; `.key(...)` captures the
    // pointer events so the App's drag handler can update `pan`.
    stack([inner])
        .key(KEY_LIGHTBOX_IMAGE)
        .layout(move |ctx: LayoutCtx| {
            // Stash the body's laid-out size for the App so a Fit→explicit-zoom
            // transition (`+`/`-` while in Fit mode) can step from the actual
            // fitted scale instead of from `1.0`. Out-of-band by necessity:
            // `Lightbox` lives in App state, not in the El tree, so its event
            // handlers have no other path to a post-layout rect today.
            if let Ok(mut size) = body_size_sink.lock() {
                *size = Some((ctx.container.w, ctx.container.h));
            }

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

/// Heuristic markdown detector. Returns `true` only when the message
/// contains unambiguous markdown syntax — paired bold/strike delimiters,
/// inline / fenced code, headings, blockquotes, lists, tables, or an
/// explicit `[label](url)` link. Stray single `*` / `_` are deliberately
/// NOT a signal (multiplication, snake_case, emphatic chat punctuation
/// trip the parser too easily) — text with only those falls through to
/// the plain-text path and still gets URL linkification.
fn looks_like_markdown(s: &str) -> bool {
    // Block-level signals, scanned per-line.
    for line in s.lines() {
        let trimmed = line.trim_start();
        // ATX heading: `# ` through `###### `.
        if let Some(rest) = trimmed.strip_prefix('#') {
            let hashes = 1 + rest.bytes().take_while(|&b| b == b'#').count();
            if hashes <= 6 && rest[hashes - 1..].starts_with(' ') {
                return true;
            }
        }
        // Blockquote / lists.
        if trimmed.starts_with("> ")
            || trimmed.starts_with("- ")
            || trimmed.starts_with("* ")
            || trimmed.starts_with("+ ")
        {
            return true;
        }
        // Ordered list: digits then ". " (or "). ").
        if let Some(idx) = trimmed.find(|c: char| !c.is_ascii_digit())
            && idx > 0
            && (trimmed[idx..].starts_with(". ") || trimmed[idx..].starts_with(") "))
        {
            return true;
        }
        // Fenced code block.
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            return true;
        }
        // Table row: at least two `|` on one line. Cheap and good
        // enough; the parser will reject malformed tables anyway.
        if trimmed.matches('|').count() >= 2 {
            return true;
        }
    }
    // Inline link: `[anything](anything)`. Crude scan — a literal `](`
    // outside link context is rare enough in chat to accept the false
    // positive rate.
    if s.contains("](") && s.contains('[') {
        return true;
    }
    // Paired multi-char inline delimiters.
    if has_paired(s, "**") || has_paired(s, "__") || has_paired(s, "~~") {
        return true;
    }
    // Inline code: at least one matched backtick pair.
    if s.matches('`').count() >= 2 {
        return true;
    }
    false
}

fn has_paired(s: &str, delim: &str) -> bool {
    s.matches(delim).count() >= 2
}

/// Build a body El for non-markdown chat text. Splits the input on
/// `linkify`-detected URLs and emails, emitting `text(slice).link(url)`
/// runs that the renderer underlines and reports clicks for via
/// `UiEventKind::LinkActivated`. Falls back to a single selectable
/// paragraph when no links are present (visually identical to the
/// pre-link rendering).
fn plain_with_links(input: &str, msg_key: u128) -> El {
    let finder = LinkFinder::new();
    let spans: Vec<_> = finder.spans(input).collect();
    let has_link = spans.iter().any(|s| s.kind().is_some());
    let key = format!("chat:msg:{msg_key}:body");

    if !has_link {
        return paragraph(input.to_string()).key(key).selectable();
    }

    let runs: Vec<El> = spans
        .into_iter()
        .map(|span| {
            let s = span.as_str().to_string();
            match span.kind() {
                Some(&LinkKind::Url) => text(s.clone()).link(s),
                Some(&LinkKind::Email) => text(s.clone()).link(format!("mailto:{s}")),
                _ => text(s),
            }
        })
        .collect();
    text_runs(runs)
        .wrap_text()
        .width(Size::Fill(1.0))
        .height(Size::Hug)
        .key(key)
        .selectable()
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

        // Fit mode is always non-overflowing (Contain clamps to body).
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
        let lb = Lightbox { fit_to_window: false, zoom: 0.3, ..lb };
        assert!(lb.image_overflows_body(huge, body));

        // Small image at 200% still fits: 200*2 = 400 < 800 → not pannable.
        let lb = Lightbox { zoom: 2.0, ..lb };
        assert!(!lb.image_overflows_body(small, body));

        // Body unknown → no pan (matches first-frame behavior).
        assert!(!lb.image_overflows_body(huge, None));
    }
}
