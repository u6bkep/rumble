//! Processing tab: the TX processor pipeline editor (with per-stage
//! live output meters) plus the RX-defaults pipeline editor applied to
//! every remote user without a per-user override.
//!
//! Both editors are instances of the shared [`crate::pipeline_ui`]
//! module; this file only decides section framing and which extras
//! (live meters, the transmit level bar) each section gets.

use super::*;
use crate::pipeline_ui;

pub(super) fn render_processing(
    pending: &PendingSettings,
    meter: &MeterSnapshot,
    outputs: &OutputFrame,
    registry: &ProcessorRegistry,
    selection: &Selection,
) -> El {
    let mut cards: Vec<El> = Vec::new();

    // ---------- TX (microphone) pipeline ----------

    // Slot assignment for live stage outputs, derived from the same config
    // the audio task derives from — so `outputs.values[slot]` lines up.
    let output_layout = OutputLayout::derive(&pending.tx_pipeline, registry);

    cards.push(section_card(
        "Processor pipeline",
        [paragraph(
            "Audio processors applied to your microphone before encoding. Drag stages with the arrow buttons to \
             change order, add or remove stages with the controls below.",
        )
        .muted()
        .font_size(tokens::TEXT_XS.size)],
    ));

    cards.extend(pipeline_ui::render_stage_cards(
        &TX_PIPELINE_KEYS,
        &pending.tx_pipeline,
        registry,
        selection,
        |idx, proc_config| {
            // Live output feedback (VAD probability meter, gate lamp).
            // Driven entirely by the stage's declared output specs — no
            // per-processor code here. Resolved against the live
            // `outputs` frame by slot. TX only: the outputs frame is
            // produced by the capture path, so the RX editor below gets
            // no live extras.
            output_layout
                .for_processor(idx)
                .map(|entry| {
                    let value = outputs.get(entry.slot).unwrap_or(0.0);
                    render_output(&entry.spec, value, &proc_config.settings)
                })
                .collect()
        },
    ));

    // "Add processor" card sits at the bottom of the list so the action
    // is visible without scrolling past meters.
    cards.push(pipeline_ui::render_add_card(&TX_PIPELINE_KEYS));

    cards.push(section_card(
        "Transmit level",
        [vu_meter(meter.input_post, Some(&pending.tx_pipeline))],
    ));

    // ---------- RX (playback) defaults ----------

    cards.push(section_card(
        "Incoming audio defaults",
        [paragraph(
            "Audio processors applied to each remote user's voice before playback. These are the defaults for \
             everyone; right-click a user and pick \"Audio…\" to override them (or adjust volume) per user.",
        )
        .muted()
        .font_size(tokens::TEXT_XS.size)],
    ));

    cards.extend(pipeline_ui::render_stage_cards(
        &RX_PIPELINE_KEYS,
        &pending.rx_pipeline,
        registry,
        selection,
        |_, _| Vec::new(),
    ));
    cards.push(pipeline_ui::render_add_card(&RX_PIPELINE_KEYS));

    column(cards).gap(tokens::SPACE_3).width(Size::Fill(1.0))
}

pub(super) fn bool_setting(proc: &ProcessorConfig, key: &str) -> bool {
    proc.settings.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}
