//! Sounds tab: master sfx toggle/volume + per-kind enable & preview.

use super::*;

pub(super) fn render_sounds(pending: &PendingSettings) -> El {
    let master = section_card(
        "Master",
        [
            field_row("Enable sound effects", switch(KEY_SFX_ENABLED, pending.sfx_enabled)),
            field_row(
                format!("Volume ({}%)", (pending.sfx_volume * 100.0).round() as i32),
                slider(KEY_SFX_VOLUME, pending.sfx_volume).width(Size::Fixed(280.0)),
            ),
        ],
    );

    let mut individual_rows: Vec<El> = Vec::new();
    for (idx, kind) in SfxKind::all().iter().enumerate() {
        let enabled = pending.sfx_kind_enabled.get(idx).copied().unwrap_or(true);
        individual_rows.push(
            row([
                text(kind.label().to_string()).label(),
                spacer(),
                button("Preview").key(sfx_preview_key(idx)).secondary(),
                switch(sfx_kind_key(idx), enabled),
            ])
            .gap(tokens::SPACE_2)
            .align(Align::Center)
            .width(Size::Fill(1.0)),
        );
    }

    column([master, section_card("Individual sounds", individual_rows)])
        .gap(tokens::SPACE_3)
        .width(Size::Fill(1.0))
}
