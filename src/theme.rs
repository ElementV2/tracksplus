//! Couleurs et style de l'interface (thème sombre).

use eframe::egui::{self, Color32, CornerRadius, FontId, Stroke, TextStyle};

pub const BG: Color32 = Color32::from_rgb(0x14, 0x16, 0x1b);
pub const PANEL_BG: Color32 = Color32::from_rgb(0x1b, 0x1e, 0x25);
pub const HEADER_BG: Color32 = Color32::from_rgb(0x20, 0x24, 0x2c);
pub const RULER_BG: Color32 = Color32::from_rgb(0x1b, 0x1e, 0x25);
pub const ROW_A: Color32 = Color32::from_rgb(0x17, 0x1a, 0x20);
pub const ROW_B: Color32 = Color32::from_rgb(0x19, 0x1c, 0x23);
pub const SEPARATOR: Color32 = Color32::from_rgb(0x2a, 0x2e, 0x38);
pub const GRID: Color32 = Color32::from_rgb(0x34, 0x39, 0x45);
pub const GRID_STRONG: Color32 = Color32::from_rgb(0x48, 0x4e, 0x5c);
pub const COUNTER_BG: Color32 = Color32::from_rgb(0x0e, 0x10, 0x14);
pub const BUTTON: Color32 = Color32::from_rgb(0x2a, 0x2f, 0x3a);
pub const BUTTON_HOVER: Color32 = Color32::from_rgb(0x36, 0x3c, 0x4a);
pub const METER_BG: Color32 = Color32::from_rgb(0x10, 0x12, 0x16);
pub const TEXT: Color32 = Color32::from_rgb(0xe6, 0xe8, 0xee);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x8a, 0x91, 0xa0);
pub const ACCENT: Color32 = Color32::from_rgb(0x4c, 0x9e, 0xff);
pub const ACCENT_DARK: Color32 = Color32::from_rgb(0x2b, 0x63, 0xb8);
pub const REC: Color32 = Color32::from_rgb(0xf0, 0x45, 0x4b);
pub const PLAY: Color32 = Color32::from_rgb(0x2f, 0xbf, 0x71);
pub const OK: Color32 = Color32::from_rgb(0x3d, 0xd6, 0x7c);
pub const WARN: Color32 = Color32::from_rgb(0xf2, 0xc1, 0x3d);
pub const PLAYHEAD: Color32 = Color32::from_rgb(0xff, 0xd3, 0x4d);
pub const TOAST: Color32 = Color32::from_rgb(0x2b, 0x63, 0xb8);
pub const TOAST_ERR: Color32 = Color32::from_rgb(0xb0, 0x2e, 0x34);

const TRACK_COLORS: [Color32; 8] = [
    Color32::from_rgb(0x4c, 0x9e, 0xff),
    Color32::from_rgb(0x3d, 0xd6, 0x9c),
    Color32::from_rgb(0xf2, 0xa1, 0x3d),
    Color32::from_rgb(0xc0, 0x7c, 0xff),
    Color32::from_rgb(0xff, 0x6f, 0x91),
    Color32::from_rgb(0x5c, 0xd6, 0xe0),
    Color32::from_rgb(0xd6, 0xd0, 0x4c),
    Color32::from_rgb(0x8f, 0xa8, 0xff),
];

pub fn track_color(i: usize) -> Color32 {
    TRACK_COLORS[i % TRACK_COLORS.len()]
}

pub fn apply(ctx: &egui::Context) {
    let mut style = (*ctx.global_style()).clone();
    let mut v = egui::Visuals::dark();
    v.panel_fill = PANEL_BG;
    v.window_fill = PANEL_BG;
    v.extreme_bg_color = COUNTER_BG;
    v.faint_bg_color = HEADER_BG;
    v.override_text_color = Some(TEXT);
    v.selection.bg_fill = ACCENT_DARK;
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.hyperlink_color = ACCENT;
    v.window_corner_radius = CornerRadius::same(10);
    v.window_stroke = Stroke::new(1.0, SEPARATOR);
    for w in [
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = CornerRadius::same(5);
    }
    v.widgets.inactive.weak_bg_fill = BUTTON;
    v.widgets.inactive.bg_fill = BUTTON;
    v.widgets.hovered.weak_bg_fill = BUTTON_HOVER;
    v.widgets.hovered.bg_fill = BUTTON_HOVER;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, SEPARATOR);
    style.visuals = v;
    style.spacing.button_padding = egui::vec2(10.0, 5.0);
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.interact_size.y = 24.0;
    style
        .text_styles
        .insert(TextStyle::Body, FontId::proportional(14.0));
    style
        .text_styles
        .insert(TextStyle::Button, FontId::proportional(14.0));
    ctx.set_global_style(style);
}
