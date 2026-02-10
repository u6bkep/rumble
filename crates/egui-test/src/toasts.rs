//! Toast notification system for the Rumble UI.
//!
//! Provides non-intrusive, auto-dismissing notifications that appear in the
//! bottom-right corner of the screen. Toasts stack vertically with newest at
//! the bottom and fade out in their last second.

use std::time::{Duration, Instant};

use eframe::egui;

/// Severity level of a toast notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastLevel {
    Success,
    Error,
    Info,
    Warning,
}

impl ToastLevel {
    fn color(&self) -> egui::Color32 {
        match self {
            ToastLevel::Success => egui::Color32::from_rgb(0x4C, 0xAF, 0x50),
            ToastLevel::Error => egui::Color32::from_rgb(0xF4, 0x43, 0x36),
            ToastLevel::Info => egui::Color32::from_rgb(0x21, 0x96, 0xF3),
            ToastLevel::Warning => egui::Color32::from_rgb(0xFF, 0x98, 0x00),
        }
    }

    fn default_duration(&self) -> Duration {
        match self {
            ToastLevel::Success | ToastLevel::Info => Duration::from_secs(4),
            ToastLevel::Error | ToastLevel::Warning => Duration::from_secs(6),
        }
    }
}

/// A single toast notification.
pub struct Toast {
    pub message: String,
    pub level: ToastLevel,
    pub created_at: Instant,
    pub duration: Duration,
}

impl Toast {
    fn new(message: impl Into<String>, level: ToastLevel) -> Self {
        Self {
            message: message.into(),
            duration: level.default_duration(),
            level,
            created_at: Instant::now(),
        }
    }

    fn elapsed(&self) -> Duration {
        self.created_at.elapsed()
    }

    fn is_expired(&self) -> bool {
        self.elapsed() >= self.duration
    }

    /// Returns opacity in 0.0..=1.0, fading out during the last second.
    fn opacity(&self) -> f32 {
        let remaining = self.duration.saturating_sub(self.elapsed());
        let remaining_secs = remaining.as_secs_f32();
        if remaining_secs < 1.0 {
            remaining_secs.max(0.0)
        } else {
            1.0
        }
    }
}

/// Manages active toast notifications and renders them each frame.
pub struct ToastManager {
    toasts: Vec<Toast>,
}

impl ToastManager {
    pub fn new() -> Self {
        Self { toasts: Vec::new() }
    }

    pub fn success(&mut self, msg: impl Into<String>) {
        self.toasts.push(Toast::new(msg, ToastLevel::Success));
    }

    pub fn error(&mut self, msg: impl Into<String>) {
        self.toasts.push(Toast::new(msg, ToastLevel::Error));
    }

    pub fn info(&mut self, msg: impl Into<String>) {
        self.toasts.push(Toast::new(msg, ToastLevel::Info));
    }

    pub fn warning(&mut self, msg: impl Into<String>) {
        self.toasts.push(Toast::new(msg, ToastLevel::Warning));
    }

    /// Render all active toasts. Call this at the end of the frame.
    ///
    /// Toasts appear in the bottom-right corner, stacked vertically with the
    /// newest at the bottom. Expired toasts are removed automatically.
    pub fn render(&mut self, ctx: &egui::Context) {
        // Remove expired toasts
        self.toasts.retain(|t| !t.is_expired());

        if self.toasts.is_empty() {
            return;
        }

        // Request repaint for fade animation
        ctx.request_repaint_after(Duration::from_millis(50));

        let screen = ctx.content_rect();
        let margin = 12.0;
        let toast_width = 320.0;
        let spacing = 6.0;

        // Calculate total height to position from bottom
        let toast_heights: Vec<f32> = self
            .toasts
            .iter()
            .map(|t| {
                // Estimate height: padding + text. Use a rough heuristic.
                let line_count = (t.message.len() as f32 / 40.0).ceil().max(1.0);
                8.0 + 8.0 + line_count * 16.0
            })
            .collect();

        let total_height: f32 =
            toast_heights.iter().sum::<f32>() + spacing * (toast_heights.len() as f32 - 1.0).max(0.0);

        let mut y = screen.max.y - margin - total_height;

        for (i, toast) in self.toasts.iter().enumerate() {
            let opacity = toast.opacity();
            let bg = toast.level.color();
            let bg = egui::Color32::from_rgba_unmultiplied(bg.r(), bg.g(), bg.b(), (opacity * 230.0) as u8);
            let text_color = egui::Color32::from_rgba_unmultiplied(255, 255, 255, (opacity * 255.0) as u8);

            let height = toast_heights[i];

            egui::Area::new(egui::Id::new("toast").with(i))
                .fixed_pos(egui::pos2(screen.max.x - margin - toast_width, y))
                .order(egui::Order::Foreground)
                .interactable(false)
                .show(ctx, |ui| {
                    let rect = egui::Rect::from_min_size(ui.cursor().min, egui::vec2(toast_width, height));
                    ui.painter().rect_filled(rect, egui::CornerRadius::same(6), bg);
                    ui.scope_builder(egui::UiBuilder::new().max_rect(rect.shrink(8.0)), |ui| {
                        ui.label(egui::RichText::new(&toast.message).color(text_color));
                    });
                    // Ensure the area takes the full size
                    ui.allocate_space(egui::vec2(toast_width, height));
                });

            y += height + spacing;
        }
    }
}
