//! Chat tab: timestamp, history-sync, and media auto-play settings.

use super::*;

pub(super) fn render_chat(pending: &PendingSettings) -> El {
    column([
        section_card(
            "Timestamps",
            [
                field_row(
                    "Show timestamps",
                    switch(pending.show_timestamps).key(KEY_CHAT_SHOW_TIMESTAMPS),
                ),
                field_row(
                    "Timestamp format",
                    select_trigger(KEY_CHAT_FORMAT, pending.timestamp_format.label()).width(Size::Fixed(280.0)),
                ),
            ],
        ),
        section_card(
            "History",
            [
                field_row(
                    "Auto-sync history on join",
                    switch(pending.auto_sync_history).key(KEY_CHAT_AUTO_SYNC),
                ),
                paragraph(
                    "Asks peers for backlog when joining a room so you can read what was said before you arrived.",
                )
                .muted()
                .font_size(tokens::TEXT_XS.size),
            ],
        ),
        section_card(
            "Media",
            [
                field_row(
                    "Auto-play animated images",
                    switch(pending.gif_autoplay).key(KEY_CHAT_GIF_AUTOPLAY),
                ),
                paragraph("Off shows the first frame with a play button overlay until you start it.")
                    .muted()
                    .font_size(tokens::TEXT_XS.size),
            ],
        ),
    ])
    .gap(tokens::SPACE_3)
    .width(Size::Fill(1.0))
}
