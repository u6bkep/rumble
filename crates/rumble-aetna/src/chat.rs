//! Chat sidebar: history rendering, FileOffer attachment cards, and
//! the composer (multi-line text area + paperclip + sync button).
//!
//! Renders the chat panel: message history, file-offer attachment
//! cards with auto-download behaviour, and the composer.
//!
//! The render side is pure: it consumes `(state, settings, chat_input,
//! selection)` and returns an `El`. The App owns event handling, slash
//! command parsing, and the async file-picker pump.

pub mod file_card;
pub mod image_preview;
pub mod lightbox;

pub use file_card::{FileContextMenu, render_file_context_menu};
pub use image_preview::{
    AnimatedTextureMap, CachedImage, GifPlayback, ImageCache, VideoThumbMap, is_image_name, is_svg_name,
};
pub use lightbox::{Lightbox, PanDrag, ZoomDirection, render_lightbox};

use std::{collections::HashMap, sync::LazyLock, time::Instant};

use aetna_core::prelude::*;
use aetna_markdown::md;
use linkify::{LinkFinder, LinkKind};
use rumble_client::{ChatMessage, ChatMessageKind, State};
use rumble_client_traits::file_transfer::TransferStatus;
use rumble_desktop_shell::ChatSettings;
use rumble_protocol::{permissions::Permissions, proto::RelayFileSharePayload};

use crate::{animated_gpu::AnimatedGpu, media_cache::MediaCache, theme as palette};

/// Active file-transfer status keyed by `transfer_id`. Built once per
/// frame from `BackendHandle::transfers()` and threaded through the
/// chat renderer so an in-flight FileOffer card can show progress
/// without each `file_offer_card` call doing its own linear search.
pub type TransferMap = HashMap<String, TransferStatus>;

/// Routed-key constants for the composer's auxiliary buttons.
pub const KEY_INPUT: &str = "chat:input";
pub const KEY_SHARE_FILE: &str = "chat:share-file";
pub const KEY_PASTE_IMAGE: &str = "chat:paste-image";
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
pub const KEY_LIGHTBOX_ZOOM_NATURAL: &str = "chat:lightbox:zoom-natural";
pub const KEY_LIGHTBOX_IMAGE: &str = "chat:lightbox:image";

/// Zoom range and step used by the lightbox controls.
pub const LIGHTBOX_ZOOM_MIN: f32 = 0.25;
pub const LIGHTBOX_ZOOM_MAX: f32 = 10.0;
pub const LIGHTBOX_ZOOM_STEP: f32 = 0.20;

static SVG_CLIPBOARD: LazyLock<SvgIcon> = LazyLock::new(|| {
    SvgIcon::parse_current_color(include_str!("../assets/icons/clipboard.svg")).expect("clipboard.svg parses")
});

/// Decode a relay-plugin file-share payload off a chat message's
/// attachment, if the attachment exists and belongs to the relay plugin.
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

/// Routed key for the explicit "open lightbox" icon overlaid on a preview.
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
pub fn parse_file_card_key(key: &str) -> Option<&str> {
    parse_plain_file_card_key(key)
        .or_else(|| parse_download_key(key))
        .or_else(|| parse_preview_key(key))
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
    if msg.visibility.is_system() && msg.attachment.is_none() {
        return paragraph(format!("{prefix}{}", msg.text))
            .font_size(tokens::TEXT_XS.size)
            .text_color(palette::CHAT_SYS)
            .italic()
            .key(format!("chat:msg:{msg_key}:sys"))
            .selectable();
    }

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

    // Messages that use markdown syntax go through the markdown renderer.
    // Everything else takes a plain-text path that linkifies bare URLs.
    let body = if looks_like_markdown(&msg.text) {
        md(&msg.text).width(Size::Fill(1.0))
    } else {
        plain_with_links(&msg.text, msg_key).width(Size::Fill(1.0))
    };

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
/// `Image` beats `Video`, `Video` beats the plain `File` card.
enum AttachmentView<'a> {
    Image {
        offer: RelayFileSharePayload,
        cached: &'a CachedImage,
        playback: Option<&'a GifPlayback>,
        gpu: Option<&'a AnimatedGpu>,
    },
    Video {
        offer: RelayFileSharePayload,
        thumb: &'a Image,
    },
    File {
        offer: RelayFileSharePayload,
        status: Option<&'a TransferStatus>,
    },
    Failed {
        offer: RelayFileSharePayload,
        reason: String,
    },
    UnknownPlugin {
        text: String,
    },
}

fn attachment_view<'a>(
    att: &rumble_protocol::ChatAttachment,
    media_cache: &'a MediaCache,
    transfers: &'a TransferMap,
) -> AttachmentView<'a> {
    use rumble_client_traits::file_transfer::TransferStage;

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
        } => image_preview::image_preview(&offer, cached, playback, gpu),
        AttachmentView::Video { offer, thumb } => image_preview::video_preview(&offer, thumb),
        AttachmentView::File { offer, status } => {
            file_card::file_offer_card(&offer, status, is_local_sender, pending_cancel_confirm)
        }
        AttachmentView::Failed { offer, reason } => file_card::failed_card(&offer, &reason, is_local_sender),
        AttachmentView::UnknownPlugin { text } => paragraph(text)
            .muted()
            .italic()
            .font_size(tokens::TEXT_XS.size)
            .selectable()
            .width(Size::Fill(1.0)),
    }
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

/// Format a byte size as a human-readable string (B / KB / MB / GB).
pub(crate) fn format_size(bytes: u64) -> String {
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
/// NOT a signal.
fn looks_like_markdown(s: &str) -> bool {
    for line in s.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix('#') {
            let hashes = 1 + rest.bytes().take_while(|&b| b == b'#').count();
            if hashes <= 6 && rest[hashes - 1..].starts_with(' ') {
                return true;
            }
        }
        if trimmed.starts_with("> ")
            || trimmed.starts_with("- ")
            || trimmed.starts_with("* ")
            || trimmed.starts_with("+ ")
        {
            return true;
        }
        if let Some(idx) = trimmed.find(|c: char| !c.is_ascii_digit())
            && idx > 0
            && (trimmed[idx..].starts_with(". ") || trimmed[idx..].starts_with(") "))
        {
            return true;
        }
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            return true;
        }
        if trimmed.matches('|').count() >= 2 {
            return true;
        }
    }
    if s.contains("](") && s.contains('[') {
        return true;
    }
    if has_paired(s, "**") || has_paired(s, "__") || has_paired(s, "~~") {
        return true;
    }
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
/// runs. Falls back to a single selectable paragraph when no links are present.
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
