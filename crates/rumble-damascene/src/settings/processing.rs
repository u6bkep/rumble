//! Processing tab: the TX processor pipeline editor + per-stage live
//! output meters.

use super::*;

pub(super) fn render_processing(
    pending: &PendingSettings,
    meter: &MeterSnapshot,
    outputs: &OutputFrame,
    registry: &ProcessorRegistry,
    selection: &Selection,
) -> El {
    let mut cards: Vec<El> = Vec::new();
    let processor_count = pending.tx_pipeline.processors.len();
    // Slot assignment for live stage outputs, derived from the same config
    // the audio task derives from — so `outputs.values[slot]` lines up.
    let output_layout = OutputLayout::derive(&pending.tx_pipeline, registry);
    // Look up display metadata once per render. `list_available` walks
    // the registry's BTreeMap and allocates a Vec, so doing it inside
    // the per-processor loop (as the previous code did) was O(n * m).
    let available: Vec<(String, String, String)> = registry
        .list_available()
        .into_iter()
        .map(|(type_id, name, desc)| (type_id.to_string(), name.to_string(), desc.to_string()))
        .collect();

    cards.push(section_card(
        "Processor pipeline",
        [paragraph(
            "Audio processors applied to your microphone before encoding. Drag stages with the arrow buttons to \
             change order, add or remove stages with the controls below.",
        )
        .muted()
        .font_size(tokens::TEXT_XS.size)],
    ));

    if processor_count == 0 {
        cards.push(section_card(
            "Processors",
            [paragraph("No processors in the pipeline. Add one below to get started.").muted()],
        ));
    }

    for (idx, proc_config) in pending.tx_pipeline.processors.iter().enumerate() {
        let (display_name, description) = available
            .iter()
            .find(|(type_id, _, _)| type_id == &proc_config.type_id)
            .map(|(_, name, desc)| (name.clone(), desc.clone()))
            .unwrap_or_else(|| (proc_config.type_id.clone(), String::new()));

        let is_first = idx == 0;
        let is_last = idx + 1 == processor_count;

        let mut move_up_btn = icon_button(IconName::ChevronUp)
            .key(proc_move_up_key(idx))
            .ghost()
            .tooltip("Move up");
        if is_first {
            move_up_btn = move_up_btn.disabled();
        }
        let mut move_down_btn = icon_button(IconName::ChevronDown)
            .key(proc_move_down_key(idx))
            .ghost()
            .tooltip("Move down");
        if is_last {
            move_down_btn = move_down_btn.disabled();
        }
        let remove_btn = icon_button(IconName::X)
            .key(proc_remove_key(idx))
            .ghost()
            .tooltip("Remove from pipeline");

        // Header row: action buttons on the left, enable switch flush
        // right. The `spacer` separates the two clusters and absorbs any
        // extra width as the dialog resizes.
        let mut body: Vec<El> = Vec::new();
        body.push(
            row([
                move_up_btn,
                move_down_btn,
                remove_btn,
                spacer(),
                switch(proc_config.enabled).key(proc_enabled_key(idx)),
            ])
            .gap(tokens::SPACE_2)
            .align(Align::Center)
            .width(Size::Fill(1.0)),
        );
        if !description.is_empty() {
            body.push(paragraph(description).muted().font_size(tokens::TEXT_XS.size));
        }
        if proc_config.enabled
            && let Some(schema) = registry.settings_schema(&proc_config.type_id)
            && let Some(properties) = schema.get("properties").and_then(|p| p.as_object())
        {
            for (key, prop_schema) in properties {
                body.push(render_schema_field(
                    idx,
                    key,
                    prop_schema,
                    &proc_config.settings,
                    selection,
                ));
            }
        }

        // Live output feedback (VAD probability meter, gate lamp). Driven
        // entirely by the stage's declared output specs — no per-processor
        // code here. Resolved against the live `outputs` frame by slot.
        if proc_config.enabled {
            for entry in output_layout.for_processor(idx) {
                let value = outputs.get(entry.slot).unwrap_or(0.0);
                body.push(render_output(&entry.spec, value, &proc_config.settings));
            }
        }

        cards.push(section_card(display_name, body));
    }

    // "Add processor" card sits at the bottom of the list so the action
    // is visible without scrolling past meters. The trigger always shows
    // the same label — picking an option fires Add and the dropdown
    // closes; there is no "selected" state to display.
    cards.push(section_card(
        "Add processor",
        [row([
            select_trigger(KEY_PROC_ADD, "Add a processor…").width(Size::Fixed(280.0)),
            spacer(),
        ])
        .gap(tokens::SPACE_2)
        .align(Align::Center)
        .width(Size::Fill(1.0))],
    ));

    cards.push(section_card(
        "Transmit level",
        [vu_meter(meter.input_post, Some(&pending.tx_pipeline))],
    ));

    column(cards).gap(tokens::SPACE_3).width(Size::Fill(1.0))
}

/// Render one schema-defined property as a labelled control. `number` /
/// `integer` types become sliders (the only general-purpose continuous
/// control damascene ships out of the box), `boolean` becomes a switch,
/// everything else falls back to a text input.
fn render_schema_field(
    proc_idx: usize,
    key: &str,
    schema: &JsonValue,
    settings: &JsonValue,
    selection: &Selection,
) -> El {
    let title = schema.get("title").and_then(|t| t.as_str()).unwrap_or(key).to_string();
    let prop_type = schema.get("type").and_then(|t| t.as_str()).unwrap_or("string");
    let route = proc_field_key(proc_idx, key);

    match prop_type {
        "number" => {
            let min = schema.get("minimum").and_then(|m| m.as_f64()).unwrap_or(-100.0) as f32;
            let max = schema.get("maximum").and_then(|m| m.as_f64()).unwrap_or(100.0) as f32;
            let default = schema.get("default").and_then(|d| d.as_f64()).unwrap_or(0.0) as f32;
            let value = settings
                .get(key)
                .and_then(|v| v.as_f64())
                .map(|v| v as f32)
                .unwrap_or(default);
            let normalized = if (max - min).abs() < f32::EPSILON {
                0.0
            } else {
                ((value - min) / (max - min)).clamp(0.0, 1.0)
            };
            field_row(
                format!("{title} ({:.2})", value),
                slider(normalized, tokens::PRIMARY)
                    .key(&route)
                    .width(Size::Fixed(220.0)),
            )
        }
        "integer" => {
            let min = schema.get("minimum").and_then(|m| m.as_i64()).unwrap_or(0);
            let max = schema.get("maximum").and_then(|m| m.as_i64()).unwrap_or(100);
            let default = schema.get("default").and_then(|d| d.as_i64()).unwrap_or(0);
            let value = settings.get(key).and_then(|v| v.as_i64()).unwrap_or(default);
            let span = (max - min).max(1) as f32;
            let normalized = ((value - min) as f32 / span).clamp(0.0, 1.0);
            field_row(
                format!("{title} ({value})"),
                slider(normalized, tokens::PRIMARY)
                    .key(&route)
                    .width(Size::Fixed(220.0)),
            )
        }
        "boolean" => {
            let default = schema.get("default").and_then(|d| d.as_bool()).unwrap_or(false);
            let value = settings.get(key).and_then(|v| v.as_bool()).unwrap_or(default);
            field_row(title, switch(value).key(&route))
        }
        _ => {
            let default = schema.get("default").and_then(|d| d.as_str()).unwrap_or("");
            let value = settings
                .get(key)
                .and_then(|v| v.as_str())
                .unwrap_or(default)
                .to_string();
            field_row(title, text_input(&value, selection, &route).width(Size::Fixed(220.0)))
        }
    }
}

pub(super) fn bool_setting(proc: &ProcessorConfig, key: &str) -> bool {
    proc.settings.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}
