//! "Mumble" theme — clone of the default Mumble Qt stylesheets.
//!
//! Light variant modeled after `reference/mumble/themes/Default/Lite.qss`:
//! Metro-flat aesthetic, white fields on a #F2F2F2 panel, 2-px radii,
//! #298CE1 azure accent used sparingly (pressed buttons, menu-item
//! highlight), soft-blue "active selection" (#DCEDF5 + #97B5C6) for
//! tree/list rows.
//!
//! Dark variant keeps the same geometry and accent hue, swaps the panel/
//! content/border greys to a neutral charcoal family that reads like the
//! stock Mumble dark stylesheet.
//!
//! No gradients, no bevels. Everything is flat fill + 1-px stroke.

use eframe::egui::{
    Color32, CornerRadius, FontFamily, FontId, Rect, Shape, Stroke, StrokeKind, Style, Visuals,
    epaint::{RectShape, Shadow},
    pos2,
};

use crate::{
    theme::Theme,
    tokens::{Axis, PressableRole, PressableState, SurfaceKind, TextRole, Tokens},
};

/// Every non-token color Mumble uses. Swapping the `&'static` reference on
/// `MumbleTheme` between `LIGHT` and `DARK` gets us a dark mode without
/// duplicating the paint code.
pub struct MumblePalette {
    // Fields that also appear on `Tokens` — duplicated here so the palette
    // is the single source of truth and `Tokens` is derived from it at
    // construction time (see `tokens_from_palette`).
    pub accent: Color32,
    pub danger: Color32,
    pub talking: Color32,
    pub surface: Color32,
    pub surface_alt: Color32,
    pub surface_sunken: Color32,
    pub text: Color32,
    pub text_muted: Color32,
    pub text_on_accent: Color32,
    pub text_on_danger: Color32,
    pub line: Color32,
    pub line_soft: Color32,

    // Non-token Mumble-specific colors.
    pub select_active_fill: Color32,
    pub select_active_border: Color32,
    pub focus_fill: Color32,
    pub focus_border: Color32,
    pub hover_border: Color32,
    pub list_hover_fill: Color32,
    pub toolbar_pressed_fill: Color32,
    pub tooltip_border: Color32,
    pub popup_fill: Color32,
    pub scroll_handle_rest: Color32,
    pub scroll_handle_hover: Color32,
    pub disabled_text: Color32,
    pub hyperlink: Color32,
    pub window_text: Color32,
}

impl MumblePalette {
    pub const LIGHT: Self = Self {
        accent: Color32::from_rgb(0x29, 0x8c, 0xe1),
        danger: Color32::from_rgb(0xc0, 0x39, 0x2b),
        talking: Color32::from_rgb(0x27, 0xae, 0x60),

        surface: Color32::from_rgb(0xff, 0xff, 0xff),
        surface_alt: Color32::from_rgb(0xfd, 0xfd, 0xfd),
        surface_sunken: Color32::from_rgb(0xf2, 0xf2, 0xf2),

        text: Color32::from_rgb(0x11, 0x11, 0x11),
        text_muted: Color32::from_rgb(0x95, 0xa5, 0xa6),
        text_on_accent: Color32::WHITE,
        text_on_danger: Color32::WHITE,

        line: Color32::from_rgb(0xc4, 0xc4, 0xc4),
        line_soft: Color32::from_rgb(0xd2, 0xd2, 0xd2),

        select_active_fill: Color32::from_rgb(0xdc, 0xed, 0xf5),
        select_active_border: Color32::from_rgb(0x97, 0xb5, 0xc6),
        focus_fill: Color32::from_rgb(0xf0, 0xfa, 0xff),
        focus_border: Color32::from_rgb(0x7e, 0xa8, 0xcc),
        hover_border: Color32::from_rgb(0xaa, 0xaa, 0xaa),
        list_hover_fill: Color32::from_rgb(0xee, 0xee, 0xee),
        toolbar_pressed_fill: Color32::from_rgb(0xdd, 0xdd, 0xdd),
        tooltip_border: Color32::from_rgb(0x66, 0x66, 0x66),
        popup_fill: Color32::from_rgb(0xf6, 0xf6, 0xf6),
        scroll_handle_rest: Color32::from_rgb(0xd6, 0xd6, 0xd6),
        scroll_handle_hover: Color32::from_rgb(0xc6, 0xc6, 0xc6),
        disabled_text: Color32::from_rgb(0xbb, 0xbb, 0xbb),
        hyperlink: Color32::from_rgb(0x0b, 0x8e, 0xb2),
        window_text: Color32::from_rgb(0x11, 0x11, 0x11),
    };

    pub const DARK: Self = Self {
        // Accent hue unchanged — it reads well on both panels.
        accent: Color32::from_rgb(0x29, 0x8c, 0xe1),
        danger: Color32::from_rgb(0xe7, 0x4c, 0x3c),
        talking: Color32::from_rgb(0x2d, 0xcc, 0x70),

        // Charcoal family: panel is darkest, content one notch lighter,
        // "alt" (Group) another notch. Keeps the same *relative*
        // contrast as light mode (content pops out of panel).
        surface: Color32::from_rgb(0x2c, 0x2c, 0x2c),
        surface_alt: Color32::from_rgb(0x32, 0x32, 0x32),
        surface_sunken: Color32::from_rgb(0x23, 0x23, 0x23),

        text: Color32::from_rgb(0xe0, 0xe0, 0xe0),
        text_muted: Color32::from_rgb(0x8a, 0x8a, 0x8a),
        text_on_accent: Color32::WHITE,
        text_on_danger: Color32::WHITE,

        // Borders: a mid-grey that's visible against #2c but doesn't
        // scream. Hard seams fall out of surface_sunken contrast, not
        // from a black border line like light mode.
        line: Color32::from_rgb(0x4a, 0x4a, 0x4a),
        line_soft: Color32::from_rgb(0x3a, 0x3a, 0x3a),

        // Selection: desaturated steel-blue that sits naturally on dark.
        select_active_fill: Color32::from_rgb(0x1f, 0x4f, 0x7a),
        select_active_border: Color32::from_rgb(0x3b, 0x6a, 0x9a),
        // Focus: same 7EA8CC-ish ring over a subtle blue tint.
        focus_fill: Color32::from_rgb(0x24, 0x34, 0x45),
        focus_border: Color32::from_rgb(0x7e, 0xa8, 0xcc),
        hover_border: Color32::from_rgb(0x6a, 0x6a, 0x6a),
        list_hover_fill: Color32::from_rgb(0x3c, 0x3c, 0x3c),
        toolbar_pressed_fill: Color32::from_rgb(0x1d, 0x1d, 0x1d),
        tooltip_border: Color32::from_rgb(0x5a, 0x5a, 0x5a),
        popup_fill: Color32::from_rgb(0x2a, 0x2a, 0x2a),
        scroll_handle_rest: Color32::from_rgb(0x4a, 0x4a, 0x4a),
        scroll_handle_hover: Color32::from_rgb(0x5c, 0x5c, 0x5c),
        disabled_text: Color32::from_rgb(0x6a, 0x6a, 0x6a),
        hyperlink: Color32::from_rgb(0x6b, 0xa6, 0xe0),
        window_text: Color32::from_rgb(0xe0, 0xe0, 0xe0),
    };
}

pub struct MumbleLiteTheme {
    palette: &'static MumblePalette,
    tokens: Tokens,
}

impl MumbleLiteTheme {
    pub fn light() -> Self {
        let palette = &MumblePalette::LIGHT;
        Self {
            palette,
            tokens: tokens_from_palette(palette),
        }
    }

    pub fn dark() -> Self {
        let palette = &MumblePalette::DARK;
        Self {
            palette,
            tokens: tokens_from_palette(palette),
        }
    }
}

impl Default for MumbleLiteTheme {
    fn default() -> Self {
        Self::light()
    }
}

fn tokens_from_palette(p: &MumblePalette) -> Tokens {
    Tokens {
        accent: p.accent,
        danger: p.danger,
        talking: p.talking,

        surface: p.surface,
        surface_alt: p.surface_alt,
        surface_sunken: p.surface_sunken,

        text: p.text,
        text_muted: p.text_muted,
        text_on_accent: p.text_on_accent,
        text_on_danger: p.text_on_danger,

        line: p.line,
        line_soft: p.line_soft,

        radius_sm: 2.0,
        radius_md: 2.0,
        radius_pill: 2.0,

        pad_sm: 4.0,
        pad_md: 8.0,

        bevel_inset: 2.0,

        font_body: FontId::new(13.0, FontFamily::Proportional),
        font_label: FontId::new(12.0, FontFamily::Proportional),
        font_heading: FontId::new(15.0, FontFamily::Proportional),
        font_mono: FontId::new(12.0, FontFamily::Monospace),
    }
}

impl Theme for MumbleLiteTheme {
    fn name(&self) -> &'static str {
        "mumble-lite"
    }

    fn tokens(&self) -> &Tokens {
        &self.tokens
    }

    fn surface(&self, rect: Rect, kind: SurfaceKind) -> Shape {
        let p = self.palette;
        let t = &self.tokens;
        let mut shapes: Vec<Shape> = Vec::new();
        match kind {
            SurfaceKind::Panel => {
                shapes.push(Shape::rect_filled(rect, 0.0, t.surface_sunken));
            }
            SurfaceKind::Pane => {
                shapes.push(Shape::rect_filled(rect, 0.0, t.surface));
                shapes.push(Shape::Rect(RectShape::stroke(
                    rect,
                    CornerRadius::ZERO,
                    Stroke::new(1.0, t.line_soft),
                    StrokeKind::Inside,
                )));
            }
            SurfaceKind::Group => {
                shapes.push(Shape::Rect(RectShape::new(
                    rect,
                    CornerRadius::from(t.radius_md),
                    t.surface_alt,
                    Stroke::new(1.0, t.line_soft),
                    StrokeKind::Inside,
                )));
            }
            SurfaceKind::Titlebar => {
                // Flat chrome matching the rest of the panel; bottom
                // hairline marks the seam below.
                shapes.push(Shape::rect_filled(rect, 0.0, t.surface_sunken));
                shapes.push(Shape::line_segment(
                    [rect.left_bottom(), rect.right_bottom()],
                    Stroke::new(1.0, t.line_soft),
                ));
            }
            SurfaceKind::Toolbar => {
                shapes.push(Shape::rect_filled(rect, 0.0, t.surface_sunken));
                shapes.push(Shape::line_segment(
                    [rect.left_bottom(), rect.right_bottom()],
                    Stroke::new(1.0, t.line_soft),
                ));
            }
            SurfaceKind::Statusbar => {
                shapes.push(Shape::rect_filled(rect, 0.0, t.surface_sunken));
                shapes.push(Shape::line_segment(
                    [rect.left_top(), rect.right_top()],
                    Stroke::new(1.0, t.line_soft),
                ));
            }
            SurfaceKind::Tooltip => {
                shapes.push(Shape::rect_filled(rect, CornerRadius::ZERO, t.surface));
                shapes.push(Shape::Rect(RectShape::stroke(
                    rect,
                    CornerRadius::ZERO,
                    Stroke::new(1.0, p.tooltip_border),
                    StrokeKind::Inside,
                )));
            }
            SurfaceKind::Popup => {
                shapes.push(Shape::Rect(RectShape::new(
                    rect,
                    CornerRadius::from(t.radius_md),
                    p.popup_fill,
                    Stroke::new(1.0, t.line_soft),
                    StrokeKind::Inside,
                )));
            }
            SurfaceKind::Field => {
                shapes.push(Shape::Rect(RectShape::new(
                    rect,
                    CornerRadius::from(t.radius_sm),
                    t.surface,
                    Stroke::new(1.0, t.line),
                    StrokeKind::Inside,
                )));
            }
        }
        Shape::Vec(shapes)
    }

    fn pressable(&self, rect: Rect, role: PressableRole, state: PressableState) -> Shape {
        let p = self.palette;
        let t = &self.tokens;
        let radius = t.radius_sm;

        // Ghost: no chrome at rest; reveals a tool-button look on hover /
        // press / active. Mirrors QToolButton from the QSS.
        let has_interaction = state.hovered || state.pressed || state.focused;
        if matches!(role, PressableRole::Ghost) && !state.active && !has_interaction {
            return Shape::Noop;
        }

        // Base fill + border by role / active. Primary uses the accent
        // fill permanently (Mumble has no "Primary" idiom but the accent
        // fill is what QPushButton:pressed uses, and that's the closest
        // Metro-flat analog).
        let (mut fill, mut border): (Color32, Color32) = match (role, state.active) {
            (PressableRole::Primary, _) => (t.accent, t.accent),
            (PressableRole::Danger, true) => (t.danger, t.danger),
            (PressableRole::Accent, true) => (p.select_active_fill, p.select_active_border),
            // Ghost-active uses the list-row hover fill so dark mode
            // shows a visible chip (in light mode this is #EEE — still
            // subtly darker than #FFF, which matches QToolButton:checked).
            (PressableRole::Ghost, true) => (p.list_hover_fill, t.line),
            (PressableRole::Default, true) => (t.surface_sunken, t.line),
            _ => (t.surface, t.line),
        };

        // Hover: QSS's `QPushButton:hover` changes only the border color;
        // fill stays the same. We mirror that for neutral roles. Ghost is
        // the QToolButton hover treatment (list-row fill).
        if state.hovered && !state.pressed && !state.disabled {
            match role {
                PressableRole::Ghost => {
                    fill = p.list_hover_fill;
                    border = t.line;
                }
                PressableRole::Primary | PressableRole::Danger => {
                    fill = lighten(fill, 0.06);
                }
                _ => {
                    if !state.active {
                        border = p.hover_border;
                    }
                }
            }
        }

        // Pressed (held): QSS presses neutral buttons into the accent
        // fill. Primary/Danger darken to keep role color recognisable.
        if state.pressed && !state.disabled {
            match role {
                PressableRole::Ghost => {
                    fill = p.toolbar_pressed_fill;
                    border = t.line;
                }
                PressableRole::Primary | PressableRole::Danger => {
                    fill = darken(fill, 0.08);
                }
                _ => {
                    fill = t.accent;
                    border = t.accent;
                }
            }
        }

        // Focus: soft blue tint + focus-ring border, only when the button
        // is at rest.
        if state.focused && !state.pressed && !state.hovered && !state.disabled {
            match role {
                PressableRole::Primary | PressableRole::Danger => {}
                _ => {
                    fill = p.focus_fill;
                    border = p.focus_border;
                }
            }
        }

        // Disabled: wash everything toward the panel background.
        if state.disabled {
            fill = blend(fill, t.surface_sunken, 0.6);
            border = blend(border, t.surface_sunken, 0.6);
        }

        let shapes = vec![
            Shape::rect_filled(rect, CornerRadius::from(radius), fill),
            Shape::Rect(RectShape::stroke(
                rect,
                CornerRadius::from(radius),
                Stroke::new(1.0, border),
                StrokeKind::Inside,
            )),
        ];
        Shape::Vec(shapes)
    }

    fn selection(&self, rect: Rect) -> Shape {
        let p = self.palette;
        Shape::Vec(vec![
            Shape::rect_filled(rect, CornerRadius::from(self.tokens.radius_sm), p.select_active_fill),
            Shape::Rect(RectShape::stroke(
                rect,
                CornerRadius::from(self.tokens.radius_sm),
                Stroke::new(1.0, p.select_active_border),
                StrokeKind::Inside,
            )),
        ])
    }

    fn separator(&self, rect: Rect, axis: Axis) -> Shape {
        let stroke = Stroke::new(1.0, self.tokens.line_soft);
        match axis {
            Axis::Horizontal => Shape::line_segment(
                [pos2(rect.left(), rect.center().y), pos2(rect.right(), rect.center().y)],
                stroke,
            ),
            Axis::Vertical => Shape::line_segment(
                [pos2(rect.center().x, rect.top()), pos2(rect.center().x, rect.bottom())],
                stroke,
            ),
        }
    }

    fn text_color(
        &self,
        _role: TextRole,
        _on: SurfaceKind,
        pressable_role: Option<PressableRole>,
        state: PressableState,
    ) -> Color32 {
        let p = self.palette;
        let t = &self.tokens;
        let base = match (pressable_role, state.active, state.pressed) {
            // Pressed neutral buttons become the accent fill — text needs
            // to flip to white to stay readable.
            (Some(PressableRole::Default), _, true)
            | (Some(PressableRole::Accent), _, true)
            | (Some(PressableRole::Ghost), _, true) => t.text_on_accent,
            (Some(PressableRole::Primary), _, _) => t.text_on_accent,
            (Some(PressableRole::Danger), true, _) => t.text_on_danger,
            _ => t.text,
        };
        if state.disabled { p.disabled_text } else { base }
    }

    fn font(&self, role: TextRole) -> FontId {
        match role {
            TextRole::Body => self.tokens.font_body.clone(),
            TextRole::Label | TextRole::Caption => self.tokens.font_label.clone(),
            TextRole::Heading => self.tokens.font_heading.clone(),
            TextRole::Mono => self.tokens.font_mono.clone(),
        }
    }

    fn apply_egui_visuals(&self, visuals: &mut Visuals) {
        let p = self.palette;
        let t = &self.tokens;
        visuals.window_fill = t.surface_sunken;
        visuals.panel_fill = t.surface_sunken;
        visuals.window_stroke = Stroke::new(1.0, t.line_soft);
        visuals.window_shadow = Shadow::NONE;
        visuals.override_text_color = Some(t.text);
        visuals.hyperlink_color = p.hyperlink;
        visuals.selection.bg_fill = t.accent;
        visuals.selection.stroke = Stroke::new(1.0, t.text_on_accent);

        // Scrollbar: flat handle on a panel-colored track, no corner
        // radius (QSS sets `border-radius: 0` on both).
        visuals.extreme_bg_color = t.surface_sunken;
        for w in [
            &mut visuals.widgets.inactive,
            &mut visuals.widgets.hovered,
            &mut visuals.widgets.active,
        ] {
            w.corner_radius = CornerRadius::ZERO;
        }
        visuals.widgets.inactive.bg_fill = p.scroll_handle_rest;
        visuals.widgets.hovered.bg_fill = p.scroll_handle_hover;
        visuals.widgets.active.bg_fill = p.scroll_handle_hover;
    }

    fn apply_egui_style(&self, style: &mut Style) {
        // Solid (always-visible) scrollbars matching the QSS's `width: 1em`
        // persistent scrollbars. A floating bar would look wrong against
        // the otherwise-chromed UI.
        let mut scroll = eframe::egui::style::ScrollStyle::solid();
        scroll.bar_width = 13.0;
        scroll.handle_min_length = 20.0;
        scroll.bar_inner_margin = 0.0;
        scroll.bar_outer_margin = 0.0;
        style.spacing.scroll = scroll;
    }
}

fn blend(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| -> u8 { (x as f32 * (1.0 - t) + y as f32 * t).round().clamp(0.0, 255.0) as u8 };
    Color32::from_rgba_unmultiplied(
        mix(a.r(), b.r()),
        mix(a.g(), b.g()),
        mix(a.b(), b.b()),
        mix(a.a(), b.a()),
    )
}

fn lighten(c: Color32, t: f32) -> Color32 {
    blend(c, Color32::WHITE, t)
}

fn darken(c: Color32, t: f32) -> Color32 {
    blend(c, Color32::BLACK, t)
}
