//! "Modern web" paradigm — top bar with logo, search pill, icon-only
//! self-state controls, and an avatar pill. Two-column body: sidebar
//! (server tree) + center (room header, chat, composer).

use crate::backend::UiBackend;
use eframe::egui::{self, Align, CornerRadius, Layout, Margin, RichText, Stroke, Ui, epaint::RectShape};
use rumble_protocol::{Command, State};
use rumble_widgets::{ButtonArgs, PressableRole, SurfaceFrame, SurfaceKind, UiExt};

use crate::{
    adapters,
    shell::{Shell, avatar_pill, room_header},
};

pub fn render<B: UiBackend>(ui: &mut Ui, shell: &mut Shell, state: &State, backend: &B) {
    top_bar(ui, shell, state, backend);

    let body = ui.available_rect_before_wrap();
    let sidebar_w = 320.0;
    let side_rect = egui::Rect::from_min_max(body.min, egui::pos2(body.min.x + sidebar_w, body.max.y));
    let center_rect = egui::Rect::from_min_max(egui::pos2(side_rect.max.x, body.min.y), body.max);

    let mut side_ui = ui.new_child(egui::UiBuilder::new().max_rect(side_rect));
    side_ui.painter().add(RectShape::filled(
        side_rect,
        CornerRadius::ZERO,
        side_ui.theme().tokens().surface_alt,
    ));
    side_ui.painter().line_segment(
        [side_rect.right_top(), side_rect.right_bottom()],
        Stroke::new(1.0, side_ui.theme().tokens().line_soft),
    );
    sidebar(&mut side_ui, shell, state, backend);

    let mut center_ui = ui.new_child(egui::UiBuilder::new().max_rect(center_rect));
    center_column(&mut center_ui, shell, state, backend);

    ui.advance_cursor_after_rect(body);
}

fn top_bar<B: UiBackend>(ui: &mut Ui, shell: &mut Shell, state: &State, backend: &B) {
    let server_name = match &state.connection {
        rumble_protocol::ConnectionState::Connected { server_name, .. } => server_name.clone(),
        _ => "disconnected".to_string(),
    };
    let self_name = adapters::my_display_name(state).unwrap_or_else(|| "you".into());

    SurfaceFrame::new(SurfaceKind::Titlebar)
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let tokens = ui.theme().tokens().clone();

                ui.label(
                    RichText::new("rumble")
                        .color(tokens.text)
                        .strong()
                        .font(tokens.font_heading.clone()),
                );
                ui.label(
                    RichText::new(format!("· {server_name}"))
                        .color(tokens.text_muted)
                        .font(tokens.font_body.clone()),
                );

                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    let avail = ui.available_width();
                    let search_w = (avail * 0.55).clamp(180.0, 360.0);
                    let side = (avail - search_w) / 2.0 - 20.0;
                    if side > 0.0 {
                        ui.add_space(side);
                    }
                    let response = egui::Frame::NONE
                        .fill(tokens.surface_sunken)
                        .corner_radius(CornerRadius::same(13))
                        .inner_margin(Margin::symmetric(12, 3))
                        .show(ui, |ui| {
                            ui.set_width(search_w);
                            ui.add(
                                egui::TextEdit::singleline(&mut shell.tree_filter)
                                    .hint_text("jump to a room or person...")
                                    .desired_width(search_w - 24.0),
                            )
                        })
                        .inner;
                    if response.changed() {
                        ui.ctx().request_repaint();
                    }
                });

                // Right-aligned self-state + avatar
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    avatar_pill(ui, &self_name, state.audio.is_transmitting);
                    ui.add_space(6.0);

                    let muted = state.audio.self_muted;
                    let deafened = state.audio.self_deafened;
                    let server_muted = adapters::am_i_server_muted(state);
                    let ptt_on = state.audio.is_transmitting;

                    // Disconnect / settings
                    if ButtonArgs::new("⎋ Disconnect")
                        .role(PressableRole::Ghost)
                        .show(ui)
                        .clicked()
                    {
                        backend.send(Command::Disconnect);
                    }
                    if ButtonArgs::new("⚙")
                        .role(PressableRole::Ghost)
                        .active(shell.settings_open)
                        .show(ui)
                        .clicked()
                    {
                        shell.settings_open = !shell.settings_open;
                    }

                    if ButtonArgs::new("🔇")
                        .role(PressableRole::Danger)
                        .active(deafened)
                        .show(ui)
                        .clicked()
                    {
                        backend.send(Command::SetDeafened { deafened: !deafened });
                    }

                    if ButtonArgs::new(if server_muted { "🔒" } else { "🎤" })
                        .role(PressableRole::Default)
                        .active(muted || server_muted)
                        .disabled(server_muted)
                        .show(ui)
                        .on_hover_text(if server_muted {
                            "Server muted — you cannot speak in this room"
                        } else if muted {
                            "Click to unmute"
                        } else {
                            "Click to mute"
                        })
                        .clicked()
                    {
                        backend.send(Command::SetMuted { muted: !muted });
                    }

                    if ButtonArgs::new("PTT")
                        .role(PressableRole::Accent)
                        .active(ptt_on && !server_muted)
                        .disabled(server_muted)
                        .show(ui)
                        .clicked()
                        && !server_muted
                    {
                        backend.send(if ptt_on {
                            Command::StopTransmit
                        } else {
                            Command::StartTransmit
                        });
                    }
                });
            });
        });
}

fn sidebar<B: UiBackend>(ui: &mut Ui, shell: &mut Shell, state: &State, backend: &B) {
    ui.horizontal(|ui| {
        let tokens = ui.theme().tokens().clone();
        ui.add_space(14.0);
        ui.label(
            RichText::new("SERVER TREE")
                .color(tokens.text_muted)
                .font(tokens.font_label.clone()),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.add_space(6.0);
            if ButtonArgs::new("+ new").role(PressableRole::Ghost).show(ui).clicked() {
                let (parent, parent_name) = current_room_parent(state);
                shell.open_create_room(parent, parent_name);
            }
        });
    });
    ui.add_space(4.0);

    egui::Frame::NONE.inner_margin(Margin::symmetric(4, 0)).show(ui, |ui| {
        shell.tree_pane(ui, state, backend);
    });
}

/// Pick the parent for a "create room" action launched from a generic
/// sidebar button: the user's current room if they're in one, else the
/// root. The display name is used purely for the modal header.
fn current_room_parent(state: &State) -> (Option<uuid::Uuid>, String) {
    match state
        .my_room_id
        .and_then(|id| state.room_tree.get(id).map(|n| (id, n.name.clone())))
    {
        Some((id, name)) => (Some(id), name),
        None => (None, "(root)".to_string()),
    }
}

fn center_column<B: UiBackend>(ui: &mut Ui, shell: &mut Shell, state: &State, backend: &B) {
    let rect = ui.available_rect_before_wrap();

    let composer_h = 56.0;
    let header_rect = egui::Rect::from_min_max(rect.min, egui::pos2(rect.max.x, rect.min.y + 64.0));
    let chat_rect = egui::Rect::from_min_max(
        egui::pos2(rect.min.x, header_rect.max.y),
        egui::pos2(rect.max.x, rect.max.y - composer_h),
    );
    let composer_rect = egui::Rect::from_min_max(egui::pos2(rect.min.x, rect.max.y - composer_h), rect.max);

    let mut hui = ui.new_child(egui::UiBuilder::new().max_rect(header_rect));
    room_header(&mut hui, state);

    let mut cui = ui.new_child(egui::UiBuilder::new().max_rect(chat_rect));
    shell.chat_stream(&mut cui, state, backend);

    let mut kui = ui.new_child(egui::UiBuilder::new().max_rect(composer_rect));
    shell.composer(&mut kui, state, backend);

    ui.advance_cursor_after_rect(rect);
}
