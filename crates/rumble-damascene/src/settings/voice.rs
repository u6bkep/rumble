//! Voice tab: voice mode, encoder, and network settings.

use super::*;

pub(super) fn render_voice(pending: &PendingSettings) -> El {
    let mode_slug = match pending.voice_mode {
        PersistentVoiceMode::PushToTalk => "ptt",
        PersistentVoiceMode::Continuous => "cont",
    };
    let bitrate_slug = bitrate_slug(pending.audio.bitrate);

    column([
        section_card(
            "Voice mode",
            [
                tabs_list(
                    KEY_VOICE_MODE_TABS,
                    &mode_slug,
                    [("ptt", "Push-to-Talk"), ("cont", "Continuous")],
                ),
                paragraph(match pending.voice_mode {
                    PersistentVoiceMode::PushToTalk => "Hold the configured PTT key (default: Space) to transmit.",
                    PersistentVoiceMode::Continuous => {
                        "Always transmitting. Add a VAD processor to gate on voice activity."
                    }
                })
                .muted()
                .font_size(tokens::TEXT_XS.size),
            ],
        ),
        section_card(
            "Encoder",
            [
                field_row(
                    "Bitrate",
                    tabs_list(
                        KEY_BITRATE_TABS,
                        &bitrate_slug,
                        [
                            ("low", "24 kbps"),
                            ("medium", "32 kbps"),
                            ("high", "64 kbps"),
                            ("very-high", "96 kbps"),
                        ],
                    )
                    .width(Size::Fixed(360.0)),
                ),
                field_row(
                    format!("Complexity ({})", pending.audio.encoder_complexity),
                    slider(pending.audio.encoder_complexity as f32 / 10.0, tokens::PRIMARY)
                        .key(KEY_VOICE_COMPLEXITY)
                        .width(Size::Fixed(280.0)),
                ),
                paragraph("Higher complexity = better quality, more CPU. Range 0–10.")
                    .muted()
                    .font_size(tokens::TEXT_XS.size),
            ],
        ),
        section_card(
            "Network",
            [
                field_row(
                    "Forward Error Correction",
                    switch(pending.audio.fec_enabled).key(KEY_VOICE_FEC),
                ),
                field_row(
                    format!(
                        "Jitter buffer ({} packets · ~{}ms)",
                        pending.audio.jitter_buffer_delay_packets,
                        pending.audio.jitter_buffer_delay_packets * 20
                    ),
                    slider(
                        (pending.audio.jitter_buffer_delay_packets as f32 - 1.0) / 9.0,
                        tokens::PRIMARY,
                    )
                    .key(KEY_VOICE_JITTER)
                    .width(Size::Fixed(280.0)),
                ),
                field_row(
                    format!("Expected packet loss ({}%)", pending.audio.packet_loss_percent),
                    slider(pending.audio.packet_loss_percent as f32 / 25.0, tokens::PRIMARY)
                        .key(KEY_VOICE_PACKET_LOSS)
                        .width(Size::Fixed(280.0)),
                ),
            ],
        ),
    ])
    .gap(tokens::SPACE_3)
    .width(Size::Fill(1.0))
}
