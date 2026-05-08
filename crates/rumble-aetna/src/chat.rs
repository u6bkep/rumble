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

use std::collections::HashMap;

use aetna_core::prelude::*;
use rumble_desktop_shell::ChatSettings;
use rumble_protocol::{ChatAttachment, ChatMessage, ChatMessageKind, FileOfferInfo, State, permissions::Permissions};

use crate::theme as palette;

/// Decoded image cache, keyed by `transfer_id`. The App maintains this
/// map and passes it through on render so the chat can swap a file
/// card for an inline preview when the underlying transfer is complete.
pub type ImageCache = HashMap<String, Image>;

/// Routed-key constants for the composer's auxiliary buttons. Send-on-
/// Enter is keyed off the input itself (`KEY_INPUT`); the buttons each
/// own their own route.
pub const KEY_INPUT: &str = "chat:input";
pub const KEY_SHARE_FILE: &str = "chat:share-file";
pub const KEY_SYNC_HISTORY: &str = "chat:sync";

/// Routed keys for the file card context menu.
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

/// Parse a `chat:download:<transfer-id>` route back to its transfer id.
pub fn parse_download_key(key: &str) -> Option<&str> {
    key.strip_prefix("chat:download:")
}

/// Parse a `chat:preview:<transfer-id>` route back to its transfer id.
pub fn parse_preview_key(key: &str) -> Option<&str> {
    key.strip_prefix("chat:preview:")
}

/// Parse a `chat:download:*` or `chat:preview:*` key back to its transfer id.
/// Used by the SecondaryClick handler to detect right-clicks on either card type.
pub fn parse_file_card_key(key: &str) -> Option<&str> {
    parse_download_key(key).or_else(|| parse_preview_key(key))
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
        KEY_FILE_CTX_DISMISS,
        menu.point,
        [
            text(menu.name.clone())
                .semibold()
                .ellipsis()
                .padding(Sides::xy(tokens::SPACE_MD, tokens::SPACE_XS)),
            divider(),
            open,
            open_folder,
            save_as,
        ],
    )
}

/// Render the chat sidebar (header, history, composer).
pub fn render(
    state: &State,
    chat_settings: &ChatSettings,
    image_cache: &ImageCache,
    chat_input: &str,
    selection: &Selection,
    width: f32,
) -> El {
    column([
        text("Chat")
            .title()
            .padding(Sides::xy(tokens::SPACE_LG, tokens::SPACE_SM)),
        divider(),
        history(state, chat_settings, image_cache),
        divider(),
        composer(state, chat_input, selection),
    ])
    .width(Size::Fixed(width))
    .height(Size::Fill(1.0))
    .fill(tokens::CARD)
}

fn history(state: &State, chat_settings: &ChatSettings, image_cache: &ImageCache) -> El {
    if state.chat_messages.is_empty() {
        let placeholder = text(if state.connection.is_connected() {
            "No messages yet"
        } else {
            "Connect to a server to start chatting"
        })
        .muted()
        .wrap_text();
        return scroll([placeholder])
            .padding(Sides::xy(tokens::SPACE_LG, tokens::SPACE_SM))
            .gap(tokens::SPACE_XS)
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0));
    }

    let lines: Vec<El> = state
        .chat_messages
        .iter()
        .map(|msg| render_message(msg, chat_settings, image_cache))
        .collect();

    scroll(lines)
        .padding(Sides::xy(tokens::SPACE_LG, tokens::SPACE_SM))
        .gap(tokens::SPACE_XS)
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0))
}

fn render_message(msg: &ChatMessage, chat_settings: &ChatSettings, image_cache: &ImageCache) -> El {
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
    // a future variant slots in cleanly.
    match msg.attachment.as_ref() {
        Some(ChatAttachment::FileOffer(offer)) => {
            let attachment = match image_cache.get(&offer.transfer_id) {
                Some(img) => image_preview(offer, img),
                None => file_offer_card(offer),
            };
            column([line, attachment]).gap(tokens::SPACE_XS).width(Size::Fill(1.0))
        }
        None => line,
    }
}

fn file_offer_card(offer: &FileOfferInfo) -> El {
    let header = row([
        icon(IconName::FileText).text_color(tokens::MUTED_FOREGROUND),
        text(offer.name.clone()).semibold().ellipsis(),
    ])
    .gap(tokens::SPACE_XS)
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

    let actions = row([
        spacer(),
        button_with_icon(IconName::Download, "Download")
            .key(download_key(&offer.transfer_id))
            .primary(),
    ])
    .width(Size::Fill(1.0))
    .align(Align::Center);

    column([header, meta, actions])
        .gap(tokens::SPACE_XS)
        .padding(Sides::all(tokens::SPACE_SM))
        .fill(tokens::SECONDARY)
        .stroke(tokens::BORDER)
        .stroke_width(1.0)
        .radius(tokens::RADIUS_MD)
        .width(Size::Fill(1.0))
}

/// Inline thumbnail card for an image attachment whose underlying
/// transfer is complete. The preview rect is fixed-height, fill-width;
/// `Contain` fit then letterboxes the source so portrait and landscape
/// images both render with the correct aspect ratio without us having
/// to know the parent column's actual width.
///
/// The whole card carries a `chat:preview:<transfer-id>` route so a
/// click anywhere on it opens the lightbox at full size. `focusable`
/// + the pointer cursor surface the affordance to keyboard and mouse
/// users alike.
fn image_preview(offer: &FileOfferInfo, img: &Image) -> El {
    const PREVIEW_HEIGHT: f32 = 220.0;

    let preview = image(img.clone())
        .image_fit(ImageFit::Contain)
        .radius(tokens::RADIUS_SM)
        .width(Size::Fill(1.0))
        .height(Size::Fixed(PREVIEW_HEIGHT));

    let caption = text(format!("{} · {}", offer.name, format_size(offer.size)))
        .muted()
        .font_size(tokens::TEXT_XS.size)
        .ellipsis();

    column([preview, caption])
        .key(preview_key(&offer.transfer_id))
        .focusable()
        .cursor(Cursor::Pointer)
        .gap(tokens::SPACE_XS)
        .padding(Sides::all(tokens::SPACE_SM))
        .fill(tokens::SECONDARY)
        .stroke(tokens::BORDER)
        .stroke_width(1.0)
        .radius(tokens::RADIUS_MD)
        .width(Size::Fill(1.0))
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

    let mut share = icon_button(IconName::Upload).key(KEY_SHARE_FILE).ghost();
    let mut sync = icon_button(IconName::RefreshCw).key(KEY_SYNC_HISTORY).ghost();
    if !connected {
        share = share.disabled();
        sync = sync.disabled();
    }

    column([
        row([input])
            .padding(Sides::xy(tokens::SPACE_LG, tokens::SPACE_SM))
            .width(Size::Fill(1.0)),
        row([share, sync, spacer()])
            .gap(tokens::SPACE_XS)
            .padding(Sides {
                left: tokens::SPACE_LG,
                right: tokens::SPACE_LG,
                top: 0.0,
                bottom: tokens::SPACE_SM,
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
/// passes it through.
pub fn render_lightbox(lightbox: &Lightbox, img: &Image) -> El {
    let header = lightbox_header(lightbox);
    let body = lightbox_body(lightbox, img);

    let panel = column([header, body])
        .style_profile(StyleProfile::Surface)
        .surface_role(SurfaceRole::Popover)
        .fill(tokens::POPOVER)
        .stroke(tokens::BORDER)
        .stroke_width(1.0)
        .radius(tokens::RADIUS_LG)
        .padding(Sides::all(tokens::SPACE_LG))
        .gap(tokens::SPACE_MD)
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0))
        .block_pointer();

    // The panel sits inside a padded container so it leaves visible
    // gutters on every side. The container is itself transparent —
    // clicks land on the scrim sibling underneath, dismissing the
    // overlay, while clicks on the panel itself are absorbed by
    // `block_pointer()`.
    let centered = stack([panel]).fill_size().padding(Sides::all(tokens::SPACE_XL));

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
    .gap(tokens::SPACE_SM)
    .align(Align::Center)
    .width(Size::Fill(1.0))
}

fn lightbox_body(lightbox: &Lightbox, img: &Image) -> El {
    // Inner image: `Contain` fit at base size; `.scale(zoom)` zooms
    // around its centre, `.translate(pan)` then offsets the result.
    // Both transforms are paint-time so layout stays stable as the
    // user drags / zooms — no re-layout cost per frame, and the
    // image's allocated rect stays the lightbox body so subsequent
    // pointer events still target the same region.
    let inner = image(img.clone())
        .image_fit(ImageFit::Contain)
        .radius(tokens::RADIUS_MD)
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0))
        .scale(lightbox.zoom)
        .translate(lightbox.pan.0, lightbox.pan.1);

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
