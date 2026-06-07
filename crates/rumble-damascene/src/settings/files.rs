//! Files tab: auto-download rules editor, bandwidth limits, and the
//! download-location picker.

use super::*;

pub(super) fn render_files(pending: &PendingSettings, selection: &Selection) -> El {
    let mut auto_dl: Vec<El> = vec![
        field_row(
            "Enable auto-download",
            switch(pending.auto_download_enabled).key(KEY_FILES_AUTO_DOWNLOAD),
        ),
        paragraph(
            "Auto-download offers whose MIME type matches a rule below, up to the per-rule size limit. Set a size of \
             `0` to keep a pattern around without it firing.",
        )
        .muted()
        .font_size(tokens::TEXT_XS.size),
        // Header row aligns with the inputs underneath. Width budgets:
        // mime grows to fill, size column is fixed (~80 px), trailing
        // remove icon is square.
        row([
            text("MIME pattern").muted().font_size(tokens::TEXT_XS.size),
            spacer(),
            text("Max size (MB)")
                .muted()
                .font_size(tokens::TEXT_XS.size)
                .width(Size::Fixed(110.0)),
            spacer().width(Size::Fixed(28.0)),
        ])
        .gap(tokens::SPACE_2)
        .align(Align::Center)
        .width(Size::Fill(1.0)),
    ];

    if pending.auto_download_rules.is_empty() {
        auto_dl.push(
            paragraph("No rules — add one below to start auto-downloading matching files.")
                .muted()
                .font_size(tokens::TEXT_XS.size),
        );
    } else {
        for (idx, rule) in pending.auto_download_rules.iter().enumerate() {
            auto_dl.push(rule_row(idx, rule, selection, pending.auto_download_enabled));
        }
    }

    let add_rule = button_with_icon(IconName::Plus, "Add rule")
        .key(KEY_FILES_RULE_ADD)
        .secondary();
    let mut action_row: Vec<El> = vec![add_rule];
    for (idx, (label, _, _)) in RULE_PRESETS.iter().enumerate() {
        action_row.push(button(format!("+ {label}")).key(preset_key(idx)).ghost());
    }
    action_row.push(spacer());
    auto_dl.push(row(action_row).gap(tokens::SPACE_2).width(Size::Fill(1.0)));

    let bandwidth = section_card(
        "Bandwidth limits",
        [
            field_row(
                format!("Download limit ({})", speed_label(pending.download_speed_kbps)),
                slider(
                    pending.download_speed_kbps as f32 / MAX_SPEED_KBPS as f32,
                    tokens::PRIMARY,
                )
                .key(KEY_FILES_DL_LIMIT)
                .width(Size::Fixed(280.0)),
            ),
            field_row(
                format!("Upload limit ({})", speed_label(pending.upload_speed_kbps)),
                slider(
                    pending.upload_speed_kbps as f32 / MAX_SPEED_KBPS as f32,
                    tokens::PRIMARY,
                )
                .key(KEY_FILES_UL_LIMIT)
                .width(Size::Fixed(280.0)),
            ),
        ],
    );

    let location = section_card(
        "Download location",
        [
            download_dir_row(pending),
            paragraph(
                "Where downloaded files are saved. Changes apply on the next connect — active transfers keep using \
                 the directory that was set when they started.",
            )
            .muted()
            .font_size(tokens::TEXT_XS.size),
        ],
    );

    column([section_card("Auto-download", auto_dl), bandwidth, location])
        .gap(tokens::SPACE_3)
        .width(Size::Fill(1.0))
}

/// Render the download-location row: path display (or a placeholder
/// for the platform default) plus the action buttons. The path text is
/// allowed to wrap so long paths don't push the buttons offscreen.
fn download_dir_row(pending: &PendingSettings) -> El {
    let (label, is_custom) = match pending.download_dir.as_ref() {
        Some(p) => (p.display().to_string(), true),
        None => ("System default (temp folder)".to_string(), false),
    };
    let path_label = mono(label)
        .font_size(tokens::TEXT_XS.size)
        .ellipsis()
        .width(Size::Fill(1.0));
    let path_label = if is_custom { path_label } else { path_label.muted() };

    let mut buttons: Vec<El> = vec![
        button("Browse…").key(KEY_FILES_DIR_BROWSE).secondary(),
        button("Open").key(KEY_FILES_DIR_OPEN).secondary(),
    ];
    if is_custom {
        buttons.push(button("Use default").key(KEY_FILES_DIR_RESET).ghost());
    }

    column([
        row([path_label]).width(Size::Fill(1.0)),
        row(buttons).gap(tokens::SPACE_2).align(Align::Center),
    ])
    .gap(tokens::SPACE_1)
    .width(Size::Fill(1.0))
}

/// One editable rule row: MIME pattern (fill width), size in MB
/// (fixed-width input), remove button. The whole row dims when
/// auto-download is off so the user gets a clear visual cue that the
/// rule list is currently inert.
fn rule_row(idx: usize, rule: &PendingAutoDownloadRule, selection: &Selection, enabled: bool) -> El {
    let mime_input = text_input_with(
        &rule.mime_pattern,
        selection,
        &rule_mime_key(idx),
        TextInputOpts::default().placeholder("e.g. image/*"),
    )
    .width(Size::Fill(1.0));
    let size_input = text_input_with(
        &rule.size_mb_text,
        selection,
        &rule_size_key(idx),
        TextInputOpts::default().placeholder("0"),
    )
    .width(Size::Fixed(110.0));
    let remove = icon_button(IconName::X)
        .key(rule_remove_key(idx))
        .ghost()
        .tooltip("Remove rule");

    let mut r = row([mime_input, size_input, remove])
        .gap(tokens::SPACE_2)
        .align(Align::Center)
        .width(Size::Fill(1.0));
    if !enabled {
        r = r.disabled();
    }
    r
}
