//! Stats tab: live encoder / network / playback counters.

use super::*;

pub(super) fn render_stats(stats: &AudioStats) -> El {
    let loss_pct = stats.packet_loss_percent();
    let loss_color = if loss_pct > 5.0 {
        tokens::DESTRUCTIVE
    } else if loss_pct > 1.0 {
        tokens::WARNING
    } else {
        tokens::SUCCESS
    };

    let encoder = section_card(
        "Encoder",
        [
            stat_row(
                "Actual bitrate",
                format!("{:.1} kbps", stats.actual_bitrate_bps / 1000.0),
                None,
            ),
            stat_row(
                "Avg frame size",
                format!("{:.1} bytes", stats.avg_frame_size_bytes),
                None,
            ),
        ],
    );

    let network = section_card(
        "Network",
        [
            stat_row("Packets sent", stats.packets_sent.to_string(), None),
            stat_row("Packets received", stats.packets_received.to_string(), None),
            stat_row(
                "Packet loss",
                format!("{:.1}% ({} lost)", loss_pct, stats.packets_lost),
                Some(loss_color),
            ),
            stat_row("FEC recovered", stats.packets_recovered_fec.to_string(), None),
        ],
    );

    // PCM playback cushion in ms (48 samples = 1 ms at 48 kHz mono). One frame
    // is 20 ms; a healthy cushion is a few frames. Underruns/overflows are
    // clicks, so any nonzero count is flagged.
    let cushion_ms = stats.playback_buffer_samples as f32 / 48.0;
    let cushion_color = if cushion_ms < 20.0 {
        tokens::DESTRUCTIVE
    } else if cushion_ms < 40.0 {
        tokens::WARNING
    } else {
        tokens::SUCCESS
    };
    let flag_color = |n: u64| if n > 0 { Some(tokens::DESTRUCTIVE) } else { None };

    let playback = section_card(
        "Playback",
        [
            stat_row("Frames concealed", stats.frames_concealed.to_string(), None),
            stat_row(
                "Jitter buffer",
                format!("{} packets", stats.playback_buffer_packets),
                None,
            ),
            stat_row(
                "PCM cushion",
                format!("{cushion_ms:.0} ms ({} samples)", stats.playback_buffer_samples),
                Some(cushion_color),
            ),
            stat_row(
                "Underruns",
                stats.buffer_underruns.to_string(),
                flag_color(stats.buffer_underruns),
            ),
            stat_row(
                "Overflows",
                stats.buffer_overflows.to_string(),
                flag_color(stats.buffer_overflows),
            ),
            row([spacer(), button("Reset statistics").key(KEY_STATS_RESET).secondary()]).width(Size::Fill(1.0)),
        ],
    );

    column([encoder, network, playback])
        .gap(tokens::SPACE_3)
        .width(Size::Fill(1.0))
}
