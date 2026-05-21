//! Gallery scene: every pressable role × state, every SurfaceKind, every
//! custom widget — rendered as a single `eframe::App`.
//!
//! Kept as a library module so both `bin/gallery.rs` (interactive) and
//! `examples/screenshot.rs` (headless via egui_kittest) can mount the
//! same composition without duplicating widget code.

use std::sync::Arc;

use eframe::egui::{self, CentralPanel, Context, Grid, ScrollArea, Vec2};

use crate::{
    Axis, ButtonArgs, ComboBox, GroupBox, LevelMeter, LunaTheme, ModernTheme, MumbleLiteTheme, Pressable,
    PressableRole, Radio, Slider, SurfaceFrame, SurfaceKind, TextInput, Toggle, ToggleStyle, Tree, TreeNode,
    TreeNodeId, UserPresence, UserState, install_theme,
    theme::{Theme, UiExt},
};

pub type ThemeFactory = fn() -> Arc<dyn Theme>;

#[derive(Clone, PartialEq, Eq)]
pub enum TransmitMode {
    Continuous,
    VoiceActivity,
    PushToTalk,
}

#[derive(Clone, PartialEq, Eq)]
pub enum NoiseSuppression {
    Disabled,
    Speex,
    RnNoise,
    Both,
}

pub struct Gallery {
    pub themes: Vec<(&'static str, ThemeFactory)>,
    pub current: usize,
    pub installed: Option<usize>,
    pub toggle_ptt: bool,
    pub toggle_mute: bool,
    pub toggle_deafen: bool,
    pub started_at: std::time::Instant,
    pub vad_threshold: f32,
    pub vad_silent: f32,
    pub vad_speech: f32,

    pub gain_db: f32,
    pub out_volume: f32,
    pub bitrate: f32,
    pub hold_ms: f32,

    pub composer: String,
    pub server_addr: String,
    pub password: String,
    pub chat_log: Vec<String>,

    pub auto_deafen: bool,
    pub push_to_talk: bool,
    pub show_self: bool,
    pub notify_on_join: bool,

    pub transmit_mode: TransmitMode,
    pub transmit_combo: usize,
    pub noise_suppression: NoiseSuppression,
    pub audio_system: usize,
    pub audio_device: usize,

    pub tree: Vec<TreeNode>,
    pub tree_selected: Option<TreeNodeId>,
    pub tree_log: String,
    pub tree_context_menu: Option<(TreeNodeId, egui::Pos2)>,
}

impl Default for Gallery {
    fn default() -> Self {
        Self {
            themes: vec![
                (
                    "Modern",
                    (|| Arc::new(ModernTheme::default()) as Arc<dyn Theme>) as ThemeFactory,
                ),
                (
                    "Luna",
                    (|| Arc::new(LunaTheme::default()) as Arc<dyn Theme>) as ThemeFactory,
                ),
                (
                    "Luna Dark",
                    (|| Arc::new(LunaTheme::dark()) as Arc<dyn Theme>) as ThemeFactory,
                ),
                (
                    "Mumble",
                    (|| Arc::new(MumbleLiteTheme::light()) as Arc<dyn Theme>) as ThemeFactory,
                ),
                (
                    "Mumble Dark",
                    (|| Arc::new(MumbleLiteTheme::dark()) as Arc<dyn Theme>) as ThemeFactory,
                ),
            ],
            current: std::env::var("GALLERY_THEME")
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(0),
            installed: None,
            toggle_ptt: false,
            toggle_mute: false,
            toggle_deafen: true,
            started_at: std::time::Instant::now(),
            vad_threshold: 0.35,
            vad_silent: 0.30,
            vad_speech: 0.55,

            gain_db: 0.0,
            out_volume: 70.0,
            bitrate: 64.0,
            hold_ms: 250.0,

            composer: String::new(),
            server_addr: String::from("voice.example.org:64738"),
            password: String::new(),
            chat_log: Vec::new(),

            auto_deafen: false,
            push_to_talk: true,
            show_self: true,
            notify_on_join: false,

            transmit_mode: TransmitMode::VoiceActivity,
            transmit_combo: 1,
            noise_suppression: NoiseSuppression::RnNoise,
            audio_system: 0,
            audio_device: 1,

            tree: sample_tree(),
            tree_selected: None,
            tree_log: String::new(),
            tree_context_menu: None,
        }
    }
}

impl eframe::App for Gallery {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        if self.installed != Some(self.current) {
            install_theme(ctx, (self.themes[self.current].1)());
            self.installed = Some(self.current);
        }

        CentralPanel::default().show(ctx, |ui| {
            ui.heading("rumble-widgets gallery");
            ui.label(format!(
                "Theme: {}. Hover / focus / click to explore states.",
                self.themes[self.current].0
            ));
            ui.add_space(6.0);

            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("theme:").small().weak());
                for (i, (name, _)) in self.themes.iter().enumerate() {
                    let clicked = ButtonArgs::new(*name)
                        .role(PressableRole::Accent)
                        .active(i == self.current)
                        .min_width(80.0)
                        .show(ui)
                        .clicked();
                    if clicked {
                        self.current = i;
                    }
                }
            });
            ui.add_space(10.0);

            ScrollArea::vertical().show(ui, |ui| {
                self.roles_grid(ui);
                ui.add_space(16.0);
                self.toggles_row(ui);
                ui.add_space(16.0);
                self.level_meters(ui);
                ui.add_space(16.0);
                self.sliders(ui);
                ui.add_space(16.0);
                self.text_inputs(ui);
                ui.add_space(16.0);
                self.toggles(ui);
                ui.add_space(16.0);
                self.presence(ui);
                ui.add_space(16.0);
                self.tree_section(ui);
                ui.add_space(16.0);
                self.group_box_section(ui);
                ui.add_space(16.0);
                self.surfaces(ui);
            });
        });
    }
}

impl Gallery {
    fn roles_grid(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Pressable — role × active").strong());
        ui.add_space(4.0);

        let roles = [
            ("Default", PressableRole::Default),
            ("Primary", PressableRole::Primary),
            ("Danger", PressableRole::Danger),
            ("Accent", PressableRole::Accent),
            ("Ghost", PressableRole::Ghost),
        ];
        let state_cols = [
            ("rest", false, false),
            ("active", true, false),
            ("disabled", false, true),
        ];

        Grid::new("roles_grid")
            .num_columns(state_cols.len() + 1)
            .spacing([16.0, 10.0])
            .show(ui, |ui| {
                ui.label("");
                for (label, _, _) in state_cols {
                    ui.label(egui::RichText::new(label).small().weak());
                }
                ui.end_row();

                for (name, role) in roles {
                    ui.label(egui::RichText::new(name).small().weak());
                    for (_, active, disabled) in state_cols {
                        ButtonArgs::new(name)
                            .role(role)
                            .active(active)
                            .disabled(disabled)
                            .min_width(72.0)
                            .show(ui);
                    }
                    ui.end_row();
                }
            });
    }

    fn toggles_row(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Stateful toggles (click to flip)").strong());
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ButtonArgs::new("PTT")
                .role(PressableRole::Accent)
                .active(self.toggle_ptt)
                .min_width(64.0)
                .show(ui)
                .clicked()
            {
                self.toggle_ptt = !self.toggle_ptt;
            }
            if ButtonArgs::new("Mute")
                .role(PressableRole::Default)
                .active(self.toggle_mute)
                .min_width(64.0)
                .show(ui)
                .clicked()
            {
                self.toggle_mute = !self.toggle_mute;
            }
            if ButtonArgs::new("Deafen")
                .role(PressableRole::Danger)
                .active(self.toggle_deafen)
                .min_width(72.0)
                .show(ui)
                .clicked()
            {
                self.toggle_deafen = !self.toggle_deafen;
            }

            let _ = Pressable::new("custom_ptt")
                .role(PressableRole::Ghost)
                .min_size(Vec2::new(120.0, 32.0))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("mic");
                        ui.label(egui::RichText::new("Space").monospace().small());
                    });
                });
        });
    }

    fn level_meters(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("LevelMeter").strong());
        ui.add_space(4.0);

        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("static:").small().weak());
            for level in [0.0, 0.25, 0.5, 0.75, 0.95] {
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new(format!("{level:.2}")).small().weak());
                    LevelMeter::new(level).min_size(Vec2::new(120.0, 14.0)).show(ui);
                });
            }
        });
        ui.add_space(8.0);

        let t = self.started_at.elapsed().as_secs_f32();
        let envelope = 0.5 + 0.45 * (t * 1.6).sin();
        let jitter = 0.05 * (t * 14.7).sin();
        let level = (envelope + jitter).clamp(0.0, 1.0);
        let peak = (level + 0.05).min(1.0);

        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("threshold:").small().weak());
            let resp = LevelMeter::new(level)
                .peak(peak)
                .threshold(self.vad_threshold)
                .interactive(true)
                .min_size(Vec2::new(280.0, 18.0))
                .show(ui);
            if let Some(t) = resp.threshold_drag {
                self.vad_threshold = t;
            }
            ui.label(
                egui::RichText::new(format!(
                    "level={level:.2}  threshold={:.2} (drag to adjust)",
                    self.vad_threshold
                ))
                .small()
                .weak(),
            );
        });

        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("vad:").small().weak());
            let resp = LevelMeter::new(level)
                .peak(peak)
                .vad(self.vad_silent, self.vad_speech)
                .interactive(true)
                .min_size(Vec2::new(280.0, 18.0))
                .show(ui);
            if let Some(t) = resp.threshold_drag {
                let d_sil = (t - self.vad_silent).abs();
                let d_spe = (t - self.vad_speech).abs();
                if d_sil <= d_spe {
                    self.vad_silent = t.min(self.vad_speech);
                } else {
                    self.vad_speech = t.max(self.vad_silent);
                }
            }
            ui.label(
                egui::RichText::new(format!("silent={:.2}  speech={:.2}", self.vad_silent, self.vad_speech))
                    .small()
                    .weak(),
            );
        });

        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("custom zones:").small().weak());
            LevelMeter::new(level)
                .zones(vec![
                    (0.40, egui::Color32::from_rgb(0x2e, 0x6f, 0xb2)),
                    (0.70, egui::Color32::from_rgb(0x4a, 0xa8, 0x4a)),
                    (0.90, egui::Color32::from_rgb(0xe0, 0xb8, 0x40)),
                    (1.00, egui::Color32::from_rgb(0xd6, 0x45, 0x45)),
                ])
                .min_size(Vec2::new(280.0, 18.0))
                .show(ui);
        });

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("vertical:").small().weak());
            for offset in [0.0, 0.4, 0.9, 1.7, 2.6] {
                let l = (0.5 + 0.45 * ((t + offset) * 1.6).sin()).clamp(0.0, 1.0);
                LevelMeter::new(l)
                    .orientation(Axis::Vertical)
                    .min_size(Vec2::new(14.0, 80.0))
                    .show(ui);
            }
        });

        ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));
    }

    fn sliders(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Slider").strong());
        ui.add_space(4.0);

        Grid::new("slider_grid")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                ui.label(egui::RichText::new("Gain (dB):").small().weak());
                Slider::new(&mut self.gain_db, -20.0..=20.0)
                    .step(1.0)
                    .suffix(" dB")
                    .show(ui);
                ui.end_row();

                ui.label(egui::RichText::new("Output volume:").small().weak());
                Slider::new(&mut self.out_volume, 0.0..=100.0)
                    .step(1.0)
                    .suffix("%")
                    .show(ui);
                ui.end_row();

                ui.label(egui::RichText::new("Bitrate:").small().weak());
                Slider::new(&mut self.bitrate, 8.0..=128.0)
                    .step(8.0)
                    .suffix(" kbps")
                    .value_box_width(64.0)
                    .show(ui);
                ui.end_row();

                ui.label(egui::RichText::new("Hold time:").small().weak());
                Slider::new(&mut self.hold_ms, 0.0..=2000.0)
                    .step(50.0)
                    .suffix(" ms")
                    .value_box_width(64.0)
                    .show(ui);
                ui.end_row();

                ui.label(egui::RichText::new("VAD threshold:").small().weak());
                Slider::new(&mut self.vad_threshold, 0.0..=1.0)
                    .step(0.01)
                    .decimals(2)
                    .show(ui);
                ui.end_row();
            });

        ui.add_space(4.0);
        ui.label(
            egui::RichText::new("Click to jump · drag to scrub · arrows / Page / Home / End when focused")
                .small()
                .weak(),
        );
    }

    fn text_inputs(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("TextInput").strong());
        ui.add_space(4.0);

        Grid::new("text_input_grid")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                ui.label(egui::RichText::new("Server:").small().weak());
                TextInput::new(&mut self.server_addr)
                    .placeholder("host:port")
                    .desired_width(240.0)
                    .id_salt("server_addr")
                    .show(ui);
                ui.end_row();

                ui.label(egui::RichText::new("Password:").small().weak());
                TextInput::new(&mut self.password)
                    .placeholder("optional")
                    .password(true)
                    .desired_width(240.0)
                    .id_salt("server_password")
                    .show(ui);
                ui.end_row();
            });

        ui.add_space(8.0);

        ui.label(egui::RichText::new("Chat composer:").small().weak());
        let resp = TextInput::new(&mut self.composer)
            .placeholder("Type a message — Enter to send, Shift+Enter for newline")
            .multiline(true)
            .submit_on_enter(true)
            .desired_width(360.0)
            .rows(2)
            .id_salt("chat_composer")
            .show(ui);
        if let Some(msg) = resp.submitted
            && !msg.trim().is_empty()
        {
            self.chat_log.push(msg);
            if self.chat_log.len() > 5 {
                let drop = self.chat_log.len() - 5;
                self.chat_log.drain(0..drop);
            }
        }

        ui.add_space(4.0);
        if self.chat_log.is_empty() {
            ui.label(egui::RichText::new("(no messages sent)").small().weak());
        } else {
            for line in &self.chat_log {
                ui.label(egui::RichText::new(format!("> {line}")).small());
            }
        }
    }

    fn toggles(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Toggle").strong());
        ui.add_space(4.0);

        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(egui::RichText::new("Switch").small().weak());
                Toggle::new(&mut self.auto_deafen, "Auto-deafen on lock").show(ui);
                Toggle::new(&mut self.push_to_talk, "Push to talk").show(ui);
                Toggle::new(&mut self.show_self, "Show self in tree").show(ui);
                Toggle::new(&mut self.notify_on_join, "Notify on user join").show(ui);
                let mut disabled_state = false;
                Toggle::new(&mut disabled_state, "Disabled (off)")
                    .disabled(true)
                    .show(ui);
                let mut disabled_on = true;
                Toggle::new(&mut disabled_on, "Disabled (on)").disabled(true).show(ui);
            });
            ui.add_space(40.0);
            ui.vertical(|ui| {
                ui.label(egui::RichText::new("Checkbox").small().weak());
                Toggle::checkbox(&mut self.auto_deafen, "Auto-deafen on lock").show(ui);
                Toggle::checkbox(&mut self.push_to_talk, "Push to talk").show(ui);
                Toggle::checkbox(&mut self.show_self, "Show self in tree").show(ui);
                Toggle::checkbox(&mut self.notify_on_join, "Notify on user join").show(ui);
                let mut disabled_state = false;
                Toggle::checkbox(&mut disabled_state, "Disabled (off)")
                    .disabled(true)
                    .show(ui);
                let mut disabled_on = true;
                Toggle::checkbox(&mut disabled_on, "Disabled (on)")
                    .style(ToggleStyle::Checkbox)
                    .disabled(true)
                    .show(ui);
            });
        });
    }

    fn presence(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("UserPresence").strong());
        ui.add_space(4.0);

        let users = [
            (
                "Alice",
                UserState {
                    talking: true,
                    ..UserState::default()
                },
            ),
            (
                "Bob",
                UserState {
                    muted: true,
                    ..UserState::default()
                },
            ),
            (
                "Carol",
                UserState {
                    deafened: true,
                    muted: true,
                    ..UserState::default()
                },
            ),
            (
                "Dan",
                UserState {
                    server_muted: true,
                    ..UserState::default()
                },
            ),
            (
                "Erin",
                UserState {
                    away: true,
                    ..UserState::default()
                },
            ),
            (
                "Frank",
                UserState {
                    talking: true,
                    deafened: true,
                    ..UserState::default()
                },
            ),
            ("Grace", UserState::default()),
        ];

        egui::Grid::new("presence_grid")
            .num_columns(1)
            .spacing([0.0, 6.0])
            .show(ui, |ui| {
                for (name, state) in users {
                    UserPresence::new().name(name).state(state).size(36.0).show(ui);
                    ui.end_row();
                }
            });
    }

    fn tree_section(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Tree").strong());
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(
                "Click rows, double-click to activate, right-click for menu, arrows + Enter to navigate, drag users \
                 into channels.",
            )
            .small()
            .weak(),
        );
        ui.add_space(6.0);

        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                Vec2::new(280.0, 320.0),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    let resp = Tree::new("gallery_tree", &self.tree)
                        .selected(self.tree_selected)
                        .drag_drop(true)
                        .show(ui);

                    if let Some(id) = resp.toggled {
                        toggle_expanded(&mut self.tree, id);
                        self.tree_log = format!("toggled {id}");
                    }
                    if let Some(id) = resp.clicked {
                        self.tree_selected = Some(id);
                        self.tree_log = format!("clicked {id}");
                    }
                    if let Some(id) = resp.double_clicked {
                        self.tree_log = format!("double-clicked {id} (would join channel)");
                    }
                    if let Some(id) = resp.activated {
                        self.tree_log = format!("activated {id} (Enter)");
                    }
                    if let Some(new_sel) = resp.selection_changed {
                        self.tree_selected = new_sel;
                        self.tree_log = format!("selection → {new_sel:?}");
                    }
                    if let Some((id, pos)) = resp.context {
                        self.tree_context_menu = Some((id, pos));
                    }
                    if let Some(drop) = resp.dropped {
                        apply_drop(&mut self.tree, drop);
                        self.tree_log = format!("dropped {} → {:?} of {}", drop.source, drop.position, drop.target);
                    }
                },
            );

            ui.add_space(12.0);
            ui.vertical(|ui| {
                ui.label(egui::RichText::new("event log").small().weak());
                ui.label(if self.tree_log.is_empty() {
                    "(interact with the tree)".to_string()
                } else {
                    self.tree_log.clone()
                });
            });
        });

        if let Some((id, pos)) = self.tree_context_menu {
            egui::Area::new(egui::Id::new("tree_ctx_menu"))
                .order(egui::Order::Foreground)
                .fixed_pos(pos)
                .show(ui.ctx(), |ui| {
                    SurfaceFrame::new(SurfaceKind::Popup)
                        .inner_margin(egui::Margin::same(6))
                        .show(ui, |ui| {
                            ui.set_min_width(140.0);
                            for label in ["Mute", "Kick", "Ban", "—", "Close"] {
                                if label == "—" {
                                    ui.separator();
                                    continue;
                                }
                                if ButtonArgs::new(label)
                                    .role(PressableRole::Ghost)
                                    .min_width(120.0)
                                    .show(ui)
                                    .clicked()
                                {
                                    self.tree_log = format!("ctx '{label}' on {id}");
                                    self.tree_context_menu = None;
                                }
                            }
                        });
                });
            if ui.input(|i| i.pointer.any_click()) && self.tree_context_menu.is_some() {
                ui.ctx().request_repaint();
            }
        }
    }

    fn group_box_section(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("GroupBox / Radio / ComboBox").strong());
        ui.add_space(4.0);

        GroupBox::new("Interface")
            .inner_margin(egui::Margin::symmetric(12, 10))
            .show(ui, |ui| {
                egui::Grid::new("group_interface")
                    .num_columns(2)
                    .spacing([12.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("System:");
                        ComboBox::new(
                            "audio_system",
                            &mut self.audio_system,
                            vec!["PipeWire".into(), "PulseAudio".into(), "ALSA".into(), "JACK".into()],
                        )
                        .width(160.0)
                        .show(ui);
                        ui.end_row();

                        ui.label("Device:");
                        ComboBox::new(
                            "audio_device",
                            &mut self.audio_device,
                            vec!["Default".into(), "Mono".into(), "Stereo".into(), "Headset".into()],
                        )
                        .width(220.0)
                        .show(ui);
                        ui.end_row();
                    });
            });

        ui.add_space(8.0);

        GroupBox::new("Transmission")
            .inner_margin(egui::Margin::symmetric(12, 10))
            .show(ui, |ui| {
                ui.label("Transmit:");
                ComboBox::new(
                    "transmit_combo",
                    &mut self.transmit_combo,
                    vec!["Continuous".into(), "Voice Activity".into(), "Push To Talk".into()],
                )
                .width(220.0)
                .show(ui);

                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    Radio::new(&mut self.transmit_mode, TransmitMode::Continuous, "Amplitude").show(ui);
                    ui.add_space(12.0);
                    Radio::new(&mut self.transmit_mode, TransmitMode::VoiceActivity, "Signal to Noise").show(ui);
                });
            });

        ui.add_space(8.0);

        GroupBox::new("Noise suppression")
            .inner_margin(egui::Margin::symmetric(12, 10))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    Radio::new(&mut self.noise_suppression, NoiseSuppression::Disabled, "Disabled").show(ui);
                    ui.add_space(12.0);
                    Radio::new(&mut self.noise_suppression, NoiseSuppression::Speex, "Speex").show(ui);
                    ui.add_space(12.0);
                    Radio::new(&mut self.noise_suppression, NoiseSuppression::RnNoise, "RNNoise").show(ui);
                    ui.add_space(12.0);
                    Radio::new(&mut self.noise_suppression, NoiseSuppression::Both, "Both").show(ui);
                });
            });
    }

    fn surfaces(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("SurfaceKind").strong());
        ui.add_space(4.0);

        let kinds = [
            ("Panel", SurfaceKind::Panel),
            ("Pane", SurfaceKind::Pane),
            ("Group", SurfaceKind::Group),
            ("Titlebar", SurfaceKind::Titlebar),
            ("Toolbar", SurfaceKind::Toolbar),
            ("Statusbar", SurfaceKind::Statusbar),
            ("Tooltip", SurfaceKind::Tooltip),
            ("Popup", SurfaceKind::Popup),
            ("Field", SurfaceKind::Field),
        ];

        egui::Grid::new("surfaces")
            .num_columns(3)
            .spacing([12.0, 12.0])
            .show(ui, |ui| {
                for (i, (name, kind)) in kinds.into_iter().enumerate() {
                    ui.vertical(|ui| {
                        ui.label(egui::RichText::new(name).small().weak());
                        SurfaceFrame::new(kind)
                            .inner_margin(egui::Margin::same(10))
                            .show(ui, |ui| {
                                ui.set_min_size(Vec2::new(180.0, 60.0));
                                let surface_text = if kind == SurfaceKind::Titlebar {
                                    ui.theme().text_color(
                                        crate::TextRole::Body,
                                        SurfaceKind::Titlebar,
                                        None,
                                        Default::default(),
                                    )
                                } else {
                                    ui.theme().tokens().text
                                };
                                ui.label(egui::RichText::new(format!("{} content", name)).color(surface_text));
                                ui.label(egui::RichText::new("example text").color(ui.theme().tokens().text_muted));
                            });
                    });
                    if (i + 1) % 3 == 0 {
                        ui.end_row();
                    }
                }
            });
    }
}

pub fn sample_tree() -> Vec<TreeNode> {
    use crate::{TreeNode as N, UserState};
    // "Lobby" is deliberately mixed — a sub-channel AND some users — so
    // the users-before-channels sort actually matters visually. The
    // backing order puts the channel first; display should show users
    // first.
    vec![
        N::channel(1, "Lobby").with_children(vec![
            N::channel(10, "Side room").with_children(vec![N::user(110, "Grace", UserState::default())]),
            N::user(
                100,
                "Alice",
                UserState {
                    talking: true,
                    ..Default::default()
                },
            ),
            N::user(
                101,
                "Bob",
                UserState {
                    muted: true,
                    ..Default::default()
                },
            ),
        ]),
        N::channel(2, "Music").with_children(vec![
            N::channel(20, "Listening").with_children(vec![N::user(200, "Carol", UserState::default())]),
            N::channel(21, "Jamming").with_children(vec![]),
        ]),
        N::channel(3, "Gaming").with_children(vec![
            N::user(
                300,
                "Dan",
                UserState {
                    deafened: true,
                    muted: true,
                    ..Default::default()
                },
            ),
            N::user(
                301,
                "Erin",
                UserState {
                    server_muted: true,
                    ..Default::default()
                },
            ),
        ]),
    ]
}

pub fn toggle_expanded(nodes: &mut [TreeNode], id: TreeNodeId) -> bool {
    for n in nodes {
        if n.id == id {
            n.expanded = !n.expanded;
            return true;
        }
        if toggle_expanded(&mut n.children, id) {
            return true;
        }
    }
    false
}

pub fn apply_drop(nodes: &mut Vec<TreeNode>, drop: crate::DropEvent) {
    let Some(source) = take_node(nodes, drop.source) else {
        return;
    };
    insert_relative(nodes, drop.target, drop.position, source);
}

fn take_node(nodes: &mut Vec<TreeNode>, id: TreeNodeId) -> Option<TreeNode> {
    if let Some(idx) = nodes.iter().position(|n| n.id == id) {
        return Some(nodes.remove(idx));
    }
    for n in nodes.iter_mut() {
        if let Some(found) = take_node(&mut n.children, id) {
            return Some(found);
        }
    }
    None
}

fn insert_relative(
    nodes: &mut Vec<TreeNode>,
    target: TreeNodeId,
    position: crate::DropPosition,
    source: TreeNode,
) -> Option<TreeNode> {
    use crate::DropPosition::{Above, Below, Into};
    let mut source = Some(source);
    if let Some(idx) = nodes.iter().position(|n| n.id == target) {
        match position {
            Into => nodes[idx].children.push(source.take().unwrap()),
            Above => nodes.insert(idx, source.take().unwrap()),
            Below => nodes.insert(idx + 1, source.take().unwrap()),
        }
        return source;
    }
    for n in nodes.iter_mut() {
        if let Some(s) = source.take() {
            source = insert_relative(&mut n.children, target, position, s);
            source.as_ref()?;
        }
    }
    source
}
