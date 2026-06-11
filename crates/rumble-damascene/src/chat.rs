//! Chat sidebar: history rendering, FileOffer attachment cards, and
//! the composer (multi-line text area + paperclip + sync button).
//!
//! Renders the chat panel: message history, file-offer attachment
//! cards with auto-download behaviour, and the composer.
//!
//! The render side is pure: it consumes `(state, settings, chat_input,
//! selection)` and returns an `El`. The App owns event handling, slash
//! command parsing, and the async file-picker pump.

pub mod composer;
pub mod file_card;
pub mod image_preview;
pub mod lightbox;

pub use composer::{ChatComposerState, ComposerOutcome};
pub use file_card::{FileContextMenu, render_file_context_menu};
pub use image_preview::{
    AnimatedTextureMap, CachedImage, GifPlayback, ImageCache, VideoThumbMap, is_image_name, is_svg_name,
};
pub use lightbox::{Lightbox, PanDrag, ZoomDirection, render_lightbox};

use std::{
    cell::RefCell,
    collections::HashMap,
    sync::{Arc, LazyLock, Mutex, RwLock},
    time::{Instant, SystemTime},
};

use damascene_core::prelude::*;
use damascene_markdown::md;
use linkify::{LinkFinder, LinkKind};
use rumble_client::{ChatMessage, ChatMessageKind, State};
use rumble_client_traits::file_transfer::TransferStatus;
use rumble_desktop_shell::ChatSettings;
use rumble_protocol::{permissions::Permissions, proto::RelayFileSharePayload};

use crate::{media_cache::MediaCache, theme as palette};

/// Active file-transfer status keyed by `transfer_id`. Built once per
/// frame from `BackendHandle::transfers()` and threaded through the
/// chat renderer so an in-flight FileOffer card can show progress
/// without each `file_offer_card` call doing its own linear search.
pub type TransferMap = HashMap<String, TransferStatus>;

/// Routed-key constants for the composer's auxiliary buttons.
pub const KEY_INPUT: &str = "chat:input";
/// Stable key for the row wrapping the text input. Without it the row's
/// `computed_id` is position-based, so inserting the slash-command suggestion
/// column above it shifts the row's index and changes the input's full-path
/// `computed_id` — which the focus reconciler reads as "node removed", clearing
/// focus the instant a `/` is typed or deleted. A key pins the id regardless of
/// siblings.
pub const KEY_INPUT_ROW: &str = "chat:input-row";
pub const KEY_SHARE_FILE: &str = "chat:share-file";
pub const KEY_PASTE_IMAGE: &str = "chat:paste-image";
pub const KEY_SYNC_HISTORY: &str = "chat:sync";

/// Key prefix for a slash-command suggestion row; the command name follows
/// (e.g. `chat:cmd:echo`). See [`command_from_key`] and [`command_suggestions`].
pub const KEY_CMD_PREFIX: &str = "chat:cmd:";

/// Client-side slash commands handled locally by the composer (see
/// `App::parse_and_send_chat`). Merged with the server-advertised commands in
/// the suggestion list so the user sees every available command in one place.
const BUILTIN_COMMANDS: &[(&str, &str)] = &[
    ("msg", "Private message — /msg <user> <text>"),
    ("tree", "Message this room and all sub-rooms — /tree <text>"),
];

/// Extract the command name from a suggestion-row key, if it is one.
pub fn command_from_key(key: &str) -> Option<&str> {
    key.strip_prefix(KEY_CMD_PREFIX)
}

/// Slash-command suggestions for the current composer line: client built-ins
/// merged with the server-advertised commands ([`State::slash_commands`]),
/// filtered by the fragment typed after `/` and sorted by name.
///
/// Returns empty unless `input` is an in-progress command — it starts with `/`
/// and has no whitespace yet (once a command and a space are typed the user is
/// entering arguments, so the list hides).
pub fn command_suggestions(input: &str, state: &State) -> Vec<(String, String)> {
    let Some(frag) = input.strip_prefix('/') else {
        return Vec::new();
    };
    if frag.contains(char::is_whitespace) {
        return Vec::new();
    }
    let frag = frag.to_ascii_lowercase();
    let mut out: Vec<(String, String)> = BUILTIN_COMMANDS
        .iter()
        .map(|(n, d)| ((*n).to_string(), (*d).to_string()))
        .chain(
            state
                .slash_commands
                .iter()
                .map(|c| (c.name.clone(), c.description.clone())),
        )
        .filter(|(name, _)| name.to_ascii_lowercase().starts_with(&frag))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out.dedup_by(|a, b| a.0.eq_ignore_ascii_case(&b.0));
    out
}

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
    history_cache: &RefCell<ChatHistory>,
    media_cache: &Arc<Mutex<MediaCache>>,
    transfers: &Arc<TransferMap>,
    pending_cancel_confirm: &HashMap<String, Instant>,
    chat_input: &str,
    selection: &Selection,
    cmd_selected: Option<usize>,
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
            history_cache,
            media_cache,
            transfers,
            pending_cancel_confirm,
            my_user_id,
            own_username,
        ),
        divider(),
        composer(state, chat_input, selection, cmd_selected),
    ])
    .height(Size::Fill(1.0))
    .fill(tokens::CARD)
}

/// Incrementally-maintained render cache for the chat history: the
/// filtered, owned [`ChatRow`] snapshots the virtual list's `'static`
/// closures index into.
///
/// The chat log is unbounded (issue #16 phase 2), so rebuilding this
/// every frame — O(log) String clones plus a relay-payload protobuf
/// decode per attachment — is the difference between flat frames and
/// frame cost growing with session age. [`Self::sync`] relies on the
/// projection's epoch contract (`State::chat_epoch`): same epoch +
/// same-or-grown length ⇒ pure append, so only the new tail is built.
/// An epoch bump (connect-time clear, history-merge re-sort) or an
/// identity change triggers the O(log) full rebuild.
///
/// Rows live behind `Arc<RwLock<…>>` so per-frame closures share the
/// same backing store instead of cloning it; appends never copy the
/// existing rows. Contention is nil (the UI thread is the only writer,
/// during `sync`; closures only read during layout, which is strictly
/// after).
#[derive(Default)]
pub struct ChatHistory {
    epoch: u64,
    /// Raw `state.chat_messages` entries consumed so far. Distinct
    /// from `rows.len()`: SenderMirror messages are filtered out.
    raw_len: usize,
    my_user_id: Option<u64>,
    own_username: String,
    rows: Arc<RwLock<Vec<ChatRow>>>,
}

impl ChatHistory {
    /// Bring the row cache up to date with `state.chat_messages` and
    /// return the shared row store for this frame's closures.
    fn sync(&mut self, state: &State, my_user_id: Option<u64>, own_username: &str) -> Arc<RwLock<Vec<ChatRow>>> {
        let msgs = &state.chat_messages;
        let appendable = state.chat_epoch == self.epoch
            && msgs.len() >= self.raw_len
            && my_user_id == self.my_user_id
            && own_username == self.own_username;
        if appendable {
            if msgs.len() > self.raw_len {
                let mut rows = self.rows.write().unwrap();
                rows.extend(
                    msgs[self.raw_len..]
                        .iter()
                        .filter(|m| m.visibility.renders_locally())
                        .map(|m| ChatRow::from_msg(m, my_user_id, own_username)),
                );
                self.raw_len = msgs.len();
            }
        } else {
            *self.rows.write().unwrap() = msgs
                .iter()
                .filter(|m| m.visibility.renders_locally())
                .map(|m| ChatRow::from_msg(m, my_user_id, own_username))
                .collect();
            self.epoch = state.chat_epoch;
            self.raw_len = msgs.len();
            self.my_user_id = my_user_id;
            self.own_username = own_username.to_string();
        }
        self.rows.clone()
    }
}

#[allow(clippy::too_many_arguments)]
fn history(
    state: &State,
    chat_settings: &ChatSettings,
    history_cache: &RefCell<ChatHistory>,
    media_cache: &Arc<Mutex<MediaCache>>,
    transfers: &Arc<TransferMap>,
    pending_cancel_confirm: &HashMap<String, Instant>,
    my_user_id: Option<u64>,
    own_username: &str,
) -> El {
    // Sync before the empty-placeholder early-out so a connect-time
    // clear (epoch bump, empty log) releases the previous session's
    // cached rows immediately instead of holding them until the new
    // server's first message.
    let rows = history_cache.borrow_mut().sync(state, my_user_id, own_username);

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
    let count = rows.read().unwrap().len();

    // Media, transfer state, and confirm timers are looked up lazily,
    // inside `build_row` — i.e. only for rows the virtual list actually
    // realizes (the visible window). The touching MediaCache accessors
    // record LRU use on hit and re-decode demand on miss right there, so
    // the byte-budget eviction (issue #37) sees off-screen backlog media
    // as evictable and an evicted entry repopulates when its row scrolls
    // back into view. That's the visible-range → cache-eviction loop of
    // issue #16, expressed through lazy realization; per-frame cost is
    // O(visible), independent of backlog length.
    let rows_for_key = rows.clone();
    let settings = chat_settings.clone();
    let media = media_cache.clone();
    let transfers = transfers.clone();
    let pending_cancel_confirm = Arc::new(pending_cancel_confirm.clone());

    virtual_list_dyn(
        count,
        CHAT_ROW_EST_HEIGHT,
        move |i| format!("chat:row:{}", rows_for_key.read().unwrap()[i].msg_key),
        move |i| {
            let rows = rows.read().unwrap();
            let row = &rows[i];
            let attachment = row
                .attachment
                .as_ref()
                .map(|att| attachment_el(att, &media, &transfers, row.is_own, &pending_cancel_confirm));
            render_chat_row(row, &settings, attachment)
        },
    )
    .key(CHAT_HISTORY_KEY)
    // Append-at-bottom feed: re-anchor rebuilds on the last visible row so new
    // messages don't shift what the user is reading. Stick-to-bottom on new
    // messages is driven separately via `ScrollRequest::ToRowKey(.., End)`.
    .virtual_anchor_policy(VirtualAnchorPolicy::LastVisible)
    .padding(Sides::xy(tokens::SPACE_4, tokens::SPACE_2))
    .gap(tokens::SPACE_1)
}

/// Estimated chat-row height (logical px) seeding the virtual list's
/// content-height total + visible-range walk for rows not yet measured. Real
/// heights are measured as rows realize; this only needs to be in the ballpark
/// of a typical 1–2 line message so the scrollbar doesn't lurch.
const CHAT_ROW_EST_HEIGHT: f32 = 44.0;

/// Key of the chat-history virtual list. Shared with the app's
/// `drain_scroll_requests` so stick-to-bottom targets the right list.
pub const CHAT_HISTORY_KEY: &str = "chat:history";

/// Stable per-message row identity for the virtual list (survives rebuilds, so
/// measurement caching + `ScrollRequest::ToRowKey` resolve to the same row).
pub(crate) fn chat_row_key(msg: &ChatMessage) -> String {
    format!("chat:row:{}", u128::from_le_bytes(msg.id))
}

/// Owned render inputs for one chat row, cloned out of the borrowed
/// `ChatMessage` so the virtual list's `build_row` closure can be
/// `'static + Send + Sync`. Built once per message ([`ChatHistory`]
/// caches rows across frames); the attachment's relay payload is
/// decoded here so realization never re-parses protobuf.
struct ChatRow {
    text: String,
    sender: String,
    kind: ChatMessageKind,
    timestamp: SystemTime,
    msg_key: u128,
    is_own: bool,
    is_system: bool,
    attachment: Option<RowAttachment>,
}

/// Static (per-message, immutable) attachment data carried by a
/// [`ChatRow`]. The *live* half — decoded media, transfer progress —
/// is looked up at realization time in [`attachment_el`].
enum RowAttachment {
    /// A relay file share with its payload already decoded.
    Relay(RelayFileSharePayload),
    /// Attachment from an unknown plugin namespace (or an undecodable
    /// payload): rendered as its fallback text.
    Unknown(String),
}

impl RowAttachment {
    fn from_attachment(att: &rumble_protocol::ChatAttachment) -> Self {
        if att.namespace != rumble_desktop::FILE_TRANSFER_RELAY_NAMESPACE {
            return RowAttachment::Unknown(att.fallback_text.clone());
        }
        match rumble_desktop::decode_relay_payload(&att.payload) {
            Some(offer) => RowAttachment::Relay(offer),
            None => RowAttachment::Unknown(att.fallback_text.clone()),
        }
    }
}

impl ChatRow {
    fn from_msg(msg: &ChatMessage, my_user_id: Option<u64>, own_username: &str) -> Self {
        // Prefer server-assigned `sender_id` for identity matching — usernames
        // are client-supplied, not unique (two clients on one machine share
        // `$USER` by default), and would mis-classify the receiver as the
        // sender. Fall back to username only when `sender_id` is absent (older
        // peers in chat-history sync, legacy servers).
        let is_system = msg.visibility.is_system();
        let is_own = if is_system {
            false
        } else if let (Some(mine), Some(theirs)) = (my_user_id, msg.sender_id) {
            mine == theirs
        } else {
            msg.sender_id.is_none() && msg.sender == own_username
        };
        ChatRow {
            text: msg.text.clone(),
            sender: msg.sender.clone(),
            kind: msg.kind.clone(),
            timestamp: msg.timestamp,
            msg_key: u128::from_le_bytes(msg.id),
            is_own,
            is_system,
            attachment: msg.attachment.as_ref().map(RowAttachment::from_attachment),
        }
    }
}

fn render_chat_row(entry: &ChatRow, chat_settings: &ChatSettings, attachment: Option<El>) -> El {
    let msg_key = entry.msg_key;
    let prefix = if chat_settings.show_timestamps {
        format!("[{}] ", chat_settings.timestamp_format.format(entry.timestamp))
    } else {
        String::new()
    };

    // System notices with no attachment: plain italic system text.
    if entry.is_system && attachment.is_none() {
        return paragraph(format!("{prefix}{}", entry.text))
            .font_size(tokens::TEXT_XS.size)
            .text_color(palette::CHAT_SYS)
            .italic()
            .key(format!("chat:msg:{msg_key}:sys"))
            .selectable();
    }

    let (header_text, header_color) = match &entry.kind {
        ChatMessageKind::Room => (format!("{prefix}{}:", entry.sender), None),
        ChatMessageKind::DirectMessage { .. } => (format!("{prefix}[DM] {}:", entry.sender), Some(palette::CHAT_DM)),
        ChatMessageKind::Tree => (format!("{prefix}[Tree] {}:", entry.sender), Some(palette::CHAT_TREE)),
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
    let body = if looks_like_markdown(&entry.text) {
        md(&entry.text).width(Size::Fill(1.0))
    } else {
        plain_with_links(&entry.text, msg_key).width(Size::Fill(1.0))
    };

    let mut parts: Vec<El> = vec![header, body];
    if let Some(att) = attachment {
        parts.push(att);
    }
    let content = column(parts).gap(tokens::SPACE_1).width(Size::Fill(1.0));

    // Own messages: subtle left accent stripe for visual differentiation.
    if entry.is_own {
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
/// Realize one attachment into an El, looking up the live half (decoded
/// media, transfer progress) at this moment. Runs inside the virtual
/// list's `build_row` — i.e. only for visible rows — which is exactly
/// what scopes the media cache's LRU to the on-screen set: the touching
/// accessors (`image_for` & co.) bump the entry's stamp on hit and
/// record re-decode demand on miss. The brief Mutex lock is uncontended
/// (layout runs on the UI thread, strictly after the App's own cache
/// access during build).
fn attachment_el(
    att: &RowAttachment,
    media_cache: &Mutex<MediaCache>,
    transfers: &TransferMap,
    is_local_sender: bool,
    pending_cancel_confirm: &HashMap<String, Instant>,
) -> El {
    use rumble_client_traits::file_transfer::TransferStage;

    let offer = match att {
        RowAttachment::Relay(offer) => offer,
        RowAttachment::Unknown(text) => {
            return paragraph(text.clone())
                .muted()
                .italic()
                .font_size(tokens::TEXT_XS.size)
                .selectable()
                .width(Size::Fill(1.0));
        }
    };

    let status = transfers.get(&offer.transfer_id);

    if let Some(s) = status
        && let TransferStage::Failed { reason } = &s.stage
    {
        return file_card::failed_card(offer, reason, is_local_sender);
    }

    let media = media_cache.lock().unwrap();
    if let Some(cached) = media.image_for(&offer.transfer_id) {
        return image_preview::image_preview(
            offer,
            cached,
            media.gif_playback_for(&offer.transfer_id),
            media.animated_gpu_for(&offer.transfer_id),
        );
    }
    if let Some(thumb) = media.video_thumb_for(&offer.transfer_id) {
        return image_preview::video_preview(offer, thumb);
    }
    if let Some(thumb) = media.model_thumb_for(&offer.transfer_id) {
        return image_preview::model_preview(offer, thumb);
    }
    drop(media);

    file_card::file_offer_card(offer, status, is_local_sender, pending_cancel_confirm)
}

fn composer(state: &State, chat_input: &str, selection: &Selection, cmd_selected: Option<usize>) -> El {
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
        .tooltip("Paste image from clipboard (Ctrl+V)");
    let mut sync = icon_button(IconName::RefreshCw)
        .key(KEY_SYNC_HISTORY)
        .ghost()
        .tooltip("Sync chat history");
    if !connected {
        share = share.disabled();
        paste = paste.disabled();
        sync = sync.disabled();
    }

    // Slash-command autocomplete: a list shown above the input while the user
    // is typing a `/command`. Clicking a row (or pressing Tab — see the App's
    // KEY_INPUT handler) completes it.
    let suggestions = if connected && can_chat {
        command_suggestions(chat_input, state)
    } else {
        Vec::new()
    };

    let mut children: Vec<El> = Vec::new();
    if !suggestions.is_empty() {
        let rows: Vec<El> = suggestions
            .iter()
            .enumerate()
            .map(|(i, (name, desc))| {
                // The arrow-key-highlighted row gets the accent fill so the
                // keyboard selection is visible; clamping to `len` happens in
                // the App, so any `Some(i)` here is in range.
                let mut r = row([
                    text(format!("/{name}")).semibold().width(Size::Fixed(120.0)),
                    text(desc.clone())
                        .muted()
                        .font_size(tokens::TEXT_XS.size)
                        .ellipsis()
                        .width(Size::Fill(1.0)),
                ])
                .key(format!("{KEY_CMD_PREFIX}{name}"))
                .focusable()
                // Dense flush rows (gap 0): draw the focus ring just inside the
                // row so a highlighted neighbor's fill can't occlude it.
                .focus_ring_inside()
                .cursor(Cursor::Pointer)
                .gap(tokens::SPACE_3)
                .padding(Sides::xy(tokens::SPACE_3, tokens::SPACE_1))
                .align(Align::Center)
                .width(Size::Fill(1.0));
                if cmd_selected == Some(i) {
                    r = r.fill(tokens::ACCENT);
                }
                r
            })
            .collect();
        children.push(
            column(rows)
                .fill(tokens::POPOVER)
                .gap(0.0)
                .width(Size::Fill(1.0))
                .padding(Sides::xy(tokens::SPACE_4, tokens::SPACE_1)),
        );
    }
    children.push(
        row([input])
            .key(KEY_INPUT_ROW)
            .padding(Sides::xy(tokens::SPACE_4, tokens::SPACE_2))
            .width(Size::Fill(1.0)),
    );
    children.push(
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
    );

    column(children).width(Size::Fill(1.0))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with(cmds: &[(&str, &str)]) -> State {
        State {
            slash_commands: cmds
                .iter()
                .map(|(n, d)| rumble_protocol::proto::SlashCommand {
                    name: (*n).to_string(),
                    description: (*d).to_string(),
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn suggestions_merge_builtins_and_server_sorted_and_filtered() {
        let s = state_with(&[("echo", "Echo bot"), ("ping", "Ping")]);

        // "/" alone matches everything: 2 builtins (msg, tree) + 2 server, sorted.
        let all = command_suggestions("/", &s);
        let names: Vec<&str> = all.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["echo", "msg", "ping", "tree"]);

        // Prefix narrows to the server command.
        let ec = command_suggestions("/ec", &s);
        let e: Vec<&str> = ec.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(e, ["echo"]);
    }

    #[test]
    fn no_suggestions_for_plain_text_or_completed_command() {
        let s = state_with(&[]);
        assert!(command_suggestions("hello", &s).is_empty());
        assert!(command_suggestions("", &s).is_empty());
        // Once a space follows the command the user is typing args — hide the list.
        assert!(command_suggestions("/msg ", &s).is_empty());
    }

    #[test]
    fn server_command_does_not_duplicate_builtin_of_same_name() {
        // A server command colliding with a builtin name collapses to one row.
        let s = state_with(&[("msg", "server msg")]);
        let result = command_suggestions("/msg", &s);
        let msgs: Vec<&str> = result.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(msgs, ["msg"]);
    }
}
