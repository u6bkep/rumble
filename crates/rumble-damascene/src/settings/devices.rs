//! Devices tab: input/output device selection + live input meter.

use super::*;

pub(super) fn render_devices(pending: &PendingSettings, audio: &AudioState, meter: &MeterSnapshot) -> El {
    let input_label = device_label_for(pending.input_device.as_deref(), &audio.input_devices);
    let output_label = device_label_for(pending.output_device.as_deref(), &audio.output_devices);

    column([
        section_card(
            "Input device",
            [
                select_trigger(KEY_INPUT_DEVICE, input_label).width(Size::Fill(1.0)),
                // Pre-pipeline level: lets the user confirm the selected
                // mic is actually capturing before they ever open the
                // Processing tab. No VAD overlay here — the threshold
                // belongs to the processing chain, not to device choice.
                vu_meter(meter.input_pre, None),
            ],
        ),
        section_card(
            "Output device",
            [select_trigger(KEY_OUTPUT_DEVICE, output_label).width(Size::Fill(1.0))],
        ),
        section_card(
            "Device list",
            [
                row([button("Refresh devices").key(KEY_REFRESH_DEVICES).secondary(), spacer()]).width(Size::Fill(1.0)),
                paragraph(
                    "Device changes apply when you click Save. Switching devices while connected may briefly drop \
                     audio.",
                )
                .muted()
                .font_size(tokens::TEXT_XS.size),
            ],
        ),
    ])
    .gap(tokens::SPACE_3)
    .width(Size::Fill(1.0))
}
