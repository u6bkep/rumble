//! Shortcuts tab: the Mumble-style keyboard-shortcut table plus the
//! Wayland RPC-socket fallback hint.

use super::*;

/// Mumble-style Shortcuts table. Header checkbox + columns
/// Function | Data | Shortcut. Function and Data are damascene select
/// dropdowns; the Shortcut cell is a button that either enters
/// capture mode (X11 / Windows / macOS) or opens the compositor's
/// system shortcut dialog (Wayland portal).
pub(super) fn render_shortcuts(pending: &PendingSettings, state: &SettingsState, hotkeys: &HotkeyManager) -> El {
    let mut shortcut_rows: Vec<El> = Vec::new();

    shortcut_rows.push(field_row(
        "Enable global shortcuts",
        switch(pending.keyboard.global_hotkeys_enabled).key(KEY_SHORTCUTS_GLOBAL_ENABLE),
    ));

    let is_wayland = hotkeys.is_wayland();
    let helper = if is_wayland {
        "Wayland binds keys through the system shortcut settings — click any Shortcut cell to open it."
    } else {
        "Click a Shortcut cell to bind a key combination; press Escape to cancel capture."
    };
    shortcut_rows.push(paragraph(helper).muted().font_size(tokens::TEXT_XS.size));

    // Column header. Width budgets match the row body below so the
    // labels line up against the dropdown triggers / shortcut buttons.
    shortcut_rows.push(
        row([
            text("Function")
                .muted()
                .font_size(tokens::TEXT_XS.size)
                .width(Size::Fixed(SHORTCUT_FN_COL_W)),
            text("Data")
                .muted()
                .font_size(tokens::TEXT_XS.size)
                .width(Size::Fixed(SHORTCUT_DATA_COL_W)),
            text("Shortcut")
                .muted()
                .font_size(tokens::TEXT_XS.size)
                .width(Size::Fill(1.0)),
            spacer().width(Size::Fixed(28.0)),
        ])
        .gap(tokens::SPACE_2)
        .align(Align::Center)
        .width(Size::Fill(1.0)),
    );

    if pending.keyboard.shortcuts.is_empty() {
        shortcut_rows.push(
            paragraph("No shortcuts configured — click Add to create one.")
                .muted()
                .font_size(tokens::TEXT_XS.size),
        );
    } else {
        for entry in &pending.keyboard.shortcuts {
            shortcut_rows.push(shortcut_row(entry, state, hotkeys, is_wayland));
        }
    }

    shortcut_rows.push(
        row([
            button_with_icon(IconName::Plus, "Add")
                .key(KEY_SHORTCUTS_ADD)
                .secondary(),
            spacer(),
        ])
        .gap(tokens::SPACE_2)
        .width(Size::Fill(1.0)),
    );

    let mut cards: Vec<El> = vec![section_card("Shortcuts", shortcut_rows)];

    if is_wayland {
        cards.push(section_card(
            "Use your desktop's shortcut settings (advanced)",
            [
                paragraph(
                    "If the portal flow above doesn't suit your compositor (tiling WMs like Sway / Hyprland, or \
                     GNOME's Custom Shortcuts), bind any key in your desktop's keyboard settings to a shell command \
                     that talks to rumble over a Unix socket. Note: most desktop shortcut systems only fire on key \
                     press, so this path supports toggle / mute / deafen well but isn't ideal for hold-style \
                     push-to-talk.",
                )
                .muted()
                .font_size(tokens::TEXT_XS.size),
                paragraph(
                    "Start damascene with `--rpc-server` to enable the socket at \
                     $XDG_RUNTIME_DIR/rumble/damascene.sock.",
                )
                .muted()
                .font_size(tokens::TEXT_XS.size),
                rpc_example_row("Toggle mute", RPC_EXAMPLE_TOGGLE_MUTE),
                rpc_example_row("Toggle deafen", RPC_EXAMPLE_TOGGLE_DEAFEN),
                rpc_example_row("Start transmit (PTT)", RPC_EXAMPLE_START_TRANSMIT),
                rpc_example_row("Stop transmit (PTT)", RPC_EXAMPLE_STOP_TRANSMIT),
            ],
        ));
    }

    column(cards).gap(tokens::SPACE_3).width(Size::Fill(1.0))
}

/// Example shell commands the user can paste into their DE's custom-
/// shortcut configuration. `nc -U` is widely available and matches the
/// RPC server's newline-delimited JSON protocol.
const RPC_EXAMPLE_TOGGLE_MUTE: &str =
    r#"echo '{"method":"set_muted","muted":true}' | nc -UN "$XDG_RUNTIME_DIR/rumble/damascene.sock""#;
const RPC_EXAMPLE_TOGGLE_DEAFEN: &str =
    r#"echo '{"method":"set_deafened","deafened":true}' | nc -UN "$XDG_RUNTIME_DIR/rumble/damascene.sock""#;
const RPC_EXAMPLE_START_TRANSMIT: &str =
    r#"echo '{"method":"start_transmit"}' | nc -UN "$XDG_RUNTIME_DIR/rumble/damascene.sock""#;
const RPC_EXAMPLE_STOP_TRANSMIT: &str =
    r#"echo '{"method":"stop_transmit"}' | nc -UN "$XDG_RUNTIME_DIR/rumble/damascene.sock""#;

/// One example row in the RPC hint: a label on the left, the command
/// text in monospace on the right (ellipsised; the user can read it in
/// the labels.txt dump if it wraps off-screen).
fn rpc_example_row(label: &str, command: &str) -> El {
    row([
        text(label)
            .muted()
            .font_size(tokens::TEXT_XS.size)
            .width(Size::Fixed(160.0)),
        mono(command)
            .font_size(tokens::TEXT_XS.size)
            .ellipsis()
            .width(Size::Fill(1.0)),
    ])
    .gap(tokens::SPACE_2)
    .align(Align::Center)
    .width(Size::Fill(1.0))
}

/// Layout constants for the shortcuts table columns. Tuned so labels
/// like "Push-to-Talk" and the longer data variants fit without
/// ellipsis.
const SHORTCUT_FN_COL_W: f32 = 160.0;
const SHORTCUT_DATA_COL_W: f32 = 110.0;

/// One row of the Shortcuts table. The trailing icon-only button
/// removes this specific row — there's no separate row-selection step,
/// which sidesteps the "selecting requires clicking the binding cell,
/// but that also opens the portal" conflict on Wayland.
fn shortcut_row(entry: &ShortcutEntry, state: &SettingsState, hotkeys: &HotkeyManager, is_wayland: bool) -> El {
    let function_trigger =
        select_trigger(shortcut_function_key(&entry.id), entry.function.label()).width(Size::Fixed(SHORTCUT_FN_COL_W));
    let data_trigger =
        select_trigger(shortcut_data_key(&entry.id), entry.data.label()).width(Size::Fixed(SHORTCUT_DATA_COL_W));

    let capturing = state.shortcut_capture.as_deref() == Some(entry.id.as_str());
    let binding_label = if capturing {
        "Press a key…".to_string()
    } else if is_wayland {
        hotkeys
            .portal_trigger(&entry.id)
            .unwrap_or_else(|| "Click to configure…".to_string())
    } else {
        entry
            .binding
            .as_ref()
            .map(|b| b.display())
            .unwrap_or_else(|| "Click to bind".to_string())
    };
    let mut binding_btn = button(binding_label).key(shortcut_binding_key(&entry.id)).secondary();
    if capturing {
        binding_btn = binding_btn.primary();
    }
    let binding_cell = binding_btn.width(Size::Fill(1.0));

    let remove = icon_button(IconName::X)
        .key(shortcut_remove_key(&entry.id))
        .ghost()
        .tooltip("Remove shortcut");

    row([function_trigger, data_trigger, binding_cell, remove])
        .gap(tokens::SPACE_2)
        .align(Align::Center)
        .width(Size::Fill(1.0))
        .key(shortcuts_row_key(&entry.id))
}
