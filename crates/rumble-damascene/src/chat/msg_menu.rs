//! Right-click context menu for chat message rows: "Copy text" (push
//! the message body onto the OS clipboard) and "Message @<author>"
//! (prefill the composer with the `/msg <username> ` DM command).
//!
//! Mirrors the file-card menu pattern ([`super::file_card`]): the App
//! captures `UiEventKind::SecondaryClick` on a message-row route,
//! snapshots what the menu needs into [`MsgContextMenu`], and renders
//! [`render_msg_context_menu`] as a floating overlay layer.

use damascene_core::prelude::*;

/// Popover id for the message context menu; the dismiss scrim is
/// derived as [`KEY_MSG_CTX_DISMISS`] by damascene's popover.
pub const KEY_MSG_CTX: &str = "chat:msg_ctx";
/// Routed keys for the message context menu actions.
pub const KEY_MSG_CTX_DISMISS: &str = "chat:msg_ctx:dismiss";
pub const KEY_MSG_CTX_COPY: &str = "chat:msg_ctx:copy";
pub const KEY_MSG_CTX_MESSAGE_AUTHOR: &str = "chat:msg_ctx:message-author";

/// Route key stamped on a chat message row's outermost element so a
/// right-click anywhere on the row (not just on a keyed child) resolves
/// to the message. Keyed children extend it (`:hdr`, `:body`, `:sys`),
/// so [`parse_msg_route`] accepts either shape.
pub fn msg_row_key(msg_key: u128) -> String {
    format!("chat:msg:{msg_key}")
}

/// Parse a `chat:msg:<key>[:suffix]` route back to its message key.
/// Matches the row wrapper and its keyed children (`:hdr`, `:body`,
/// `:sys`); returns `None` for every other route.
pub fn parse_msg_route(route: &str) -> Option<u128> {
    let rest = route.strip_prefix("chat:msg:")?;
    rest.split(':').next()?.parse().ok()
}

/// State for the right-click message context menu. Snapshotted at open
/// time (same pattern as [`super::FileContextMenu`]); `Some` on the App
/// while open, cleared by any action, the dismiss scrim, or Escape.
#[derive(Clone, Debug)]
pub struct MsgContextMenu {
    /// Message body — what "Copy text" puts on the clipboard.
    pub text: String,
    /// Author username as carried on the message.
    pub sender: String,
    /// Whether "Message @<sender>" is offered: the author resolved to a
    /// connected remote user when the menu opened. The App re-checks
    /// resolvability at click time so a stale menu can't prefill a DM
    /// to a user who has since left.
    pub can_dm: bool,
    /// Screen-space anchor for the context menu popover.
    pub point: (f32, f32),
}

/// Render the message context menu as a floating popover overlay.
pub fn render_msg_context_menu(menu: &MsgContextMenu) -> El {
    let mut items: Vec<El> = Vec::new();
    if !menu.sender.is_empty() {
        items.push(
            text(menu.sender.clone())
                .semibold()
                .ellipsis()
                .padding(Sides::xy(tokens::SPACE_3, tokens::SPACE_1)),
        );
        items.push(divider());
    }
    items.push(menu_item("Copy text").key(KEY_MSG_CTX_COPY));
    if menu.can_dm {
        items.push(menu_item(format!("Message @{}", menu.sender)).key(KEY_MSG_CTX_MESSAGE_AUTHOR));
    }
    context_menu(KEY_MSG_CTX, menu.point, items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_row_and_child_routes() {
        let k = 42u128 << 96;
        assert_eq!(parse_msg_route(&msg_row_key(k)), Some(k));
        assert_eq!(parse_msg_route(&format!("chat:msg:{k}:hdr")), Some(k));
        assert_eq!(parse_msg_route(&format!("chat:msg:{k}:body")), Some(k));
        assert_eq!(parse_msg_route(&format!("chat:msg:{k}:sys")), Some(k));
    }

    #[test]
    fn parse_rejects_other_routes() {
        assert_eq!(parse_msg_route("chat:msg_ctx:copy"), None);
        assert_eq!(parse_msg_route("chat:row:7"), None);
        assert_eq!(parse_msg_route("chat:msg:not-a-number"), None);
    }
}
