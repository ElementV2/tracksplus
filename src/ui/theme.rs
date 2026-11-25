//! Direction artistique : sombre, plate et dense. Gris anthracite neutres, séparateurs
//! fins, angles presque droits, un seul accent orange, couleurs de pistes désaturées.

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Shadow, Stroke,
    TextStyle,
};
use std::sync::Arc;

// Fonds, du plus profond au plus clair.
pub const BG: Color32 = Color32::from_rgb(0x14, 0x14, 0x14);
pub const SURFACE: Color32 = Color32::from_rgb(0x1e, 0x1e, 0x1e);
pub const SURFACE_2: Color32 = Color32::from_rgb(0x26, 0x26, 0x26);
pub const SURFACE_3: Color32 = Color32::from_rgb(0x30, 0x30, 0x30);
pub const SURFACE_4: Color32 = Color32::from_rgb(0x3b, 0x3b, 0x3b);
/// Trait sombre entre les zones.
pub const LINE: Color32 = Color32::from_rgb(0x0a, 0x0a, 0x0a);
pub const LANE_A: Color32 = Color32::from_rgb(0x1a, 0x1a, 0x1a);
pub const LANE_B: Color32 = Color32::from_rgb(0x1c, 0x1c, 0x1c);
pub const GRID: Color32 = Color32::from_rgb(0x24, 0x24, 0x24);
pub const INSET: Color32 = Color32::from_rgb(0x10, 0x10, 0x10);

pub const TEXT: Color32 = Color32::from_rgb(0xdc, 0xdc, 0xdc);
pub const TEXT_2: Color32 = Color32::from_rgb(0x9c, 0x9c, 0x9c);
pub const TEXT_3: Color32 = Color32::from_rgb(0x66, 0x66, 0x66);

pub const ACCENT: Color32 = Color32::from_rgb(0xff, 0x8a, 0x1f);
pub const ACCENT_DIM: Color32 = Color32::from_rgb(0x5a, 0x35, 0x12);
pub const REC: Color32 = Color32::from_rgb(0xe8, 0x45, 0x3c);
pub const PLAY: Color32 = Color32::from_rgb(0x5f, 0xc8, 0x5c);
pub const YELLOW: Color32 = Color32::from_rgb(0xe6, 0xc2, 0x3a);
pub const ORANGE: Color32 = Color32::from_rgb(0xf0, 0x9a, 0x2e);
pub const PLAYHEAD: Color32 = Color32::from_rgb(0xf0, 0xf0, 0xf0);
pub const SELECTION: Color32 = Color32::from_rgba_unmultiplied_const(0xff, 0xff, 0xff, 0x12);
pub const MARKER: Color32 = Color32::from_rgb(0xc9, 0xa2, 0x4a);

pub const METER_OFF: Color32 = Color32::from_rgb(0x0e, 0x0e, 0x0e);
pub const METER_GREEN: Color32 = Color32::from_rgb(0x57, 0xc0, 0x4f);
pub const METER_YELLOW: Color32 = Color32::from_rgb(0xdc, 0xbd, 0x37);
pub const METER_RED: Color32 = Color32::from_rgb(0xe8, 0x45, 0x3c);

/// Couleurs de pistes et de pads, désaturées pour rester lisibles sur fond sombre.
pub const PALETTE: [Color32; 12] = [
    Color32::from_rgb(0xd9, 0x5e, 0x45),
    Color32::from_rgb(0xe0, 0x8a, 0x35),
    Color32::from_rgb(0xcf, 0xb5, 0x41),
    Color32::from_rgb(0x8f, 0xb4, 0x45),
    Color32::from_rgb(0x4c, 0xa9, 0x6b),
    Color32::from_rgb(0x3f, 0xa8, 0x9f),
    Color32::from_rgb(0x46, 0x9a, 0xd0),
    Color32::from_rgb(0x6f, 0x80, 0xd6),
    Color32::from_rgb(0x95, 0x72, 0xcc),
    Color32::from_rgb(0xc0, 0x63, 0xa6),
    Color32::from_rgb(0x9a, 0x8a, 0x78),
    Color32::from_rgb(0x8a, 0x93, 0x9b),
];

pub fn color(i: usize) -> Color32 {
    PALETTE[(i + 6) % PALETTE.len()]
}

pub fn mix(a: Color32, b: Color32, k: f32) -> Color32 {
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * k).round() as u8;
    Color32::from_rgb(m(a.r(), b.r()), m(a.g(), b.g()), m(a.b(), b.b()))
}

/// Rayons : presque droits.
pub const R: u8 = 2;
pub const R_PANEL: u8 = 3;

pub fn medium(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("medium".into()))
}

pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("semibold".into()))
}

pub fn regular(size: f32) -> FontId {
    FontId::proportional(size)
}

/// Libellé de section en petites capitales.
pub fn caps(size: f32) -> FontId {
    semibold(size)
}

pub fn window_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(SURFACE)
        .stroke(Stroke::new(1.0, LINE))
        .corner_radius(R_PANEL)
        .inner_margin(egui::Margin::same(16))
        .shadow(Shadow {
            offset: [0, 8],
            blur: 24,
            spread: 0,
            color: Color32::from_black_alpha(160),
        })
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let add = |fonts: &mut FontDefinitions, name: &str, bytes: &'static [u8]| {
        fonts
            .font_data
            .insert(name.to_owned(), Arc::new(FontData::from_static(bytes)));
    };
    add(
        &mut fonts,
        "Inter",
        include_bytes!("../../assets/fonts/Inter-Regular.ttf"),
    );
    add(
        &mut fonts,
        "Inter-Medium",
        include_bytes!("../../assets/fonts/Inter-Medium.ttf"),
    );
    add(
        &mut fonts,
        "Inter-SemiBold",
        include_bytes!("../../assets/fonts/Inter-SemiBold.ttf"),
    );
    let fallback: Vec<String> = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, "Inter".to_owned());
    for (fam, font) in [("medium", "Inter-Medium"), ("semibold", "Inter-SemiBold")] {
        let mut list = vec![font.to_owned()];
        list.extend(fallback.iter().cloned());
        fonts.families.insert(FontFamily::Name(fam.into()), list);
    }
    ctx.set_fonts(fonts);
}

pub fn apply(ctx: &egui::Context) {
    install_fonts(ctx);
    // Molette : Ctrl = zoom, Alt = défilement horizontal (dans le temps), Maj = vertical.
    ctx.options_mut(|o| {
        o.input_options.horizontal_scroll_modifier = egui::Modifiers::ALT;
        o.input_options.vertical_scroll_modifier = egui::Modifiers::SHIFT;
    });
    let mut style = (*ctx.global_style()).clone();
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = SURFACE;
    v.extreme_bg_color = INSET;
    v.faint_bg_color = SURFACE_2;
    v.code_bg_color = INSET;
    v.override_text_color = Some(TEXT);
    v.selection.bg_fill = ACCENT_DIM;
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.hyperlink_color = ACCENT;
    v.window_corner_radius = CornerRadius::same(R_PANEL);
    v.window_stroke = Stroke::new(1.0, LINE);
    v.menu_corner_radius = CornerRadius::same(R_PANEL);
    v.popup_shadow = Shadow {
        offset: [0, 4],
        blur: 14,
        spread: 0,
        color: Color32::from_black_alpha(150),
    };
    v.window_shadow = v.popup_shadow;
    v.text_cursor.stroke = Stroke::new(1.5, ACCENT);
    let w = &mut v.widgets;
    w.noninteractive.bg_stroke = Stroke::new(1.0, LINE);
    w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT_2);
    w.noninteractive.corner_radius = CornerRadius::same(R);
    w.noninteractive.bg_fill = SURFACE;
    for (s, fill) in [
        (&mut w.inactive, SURFACE_3),
        (&mut w.hovered, SURFACE_4),
        (&mut w.active, SURFACE_4),
        (&mut w.open, SURFACE_4),
    ] {
        s.corner_radius = CornerRadius::same(R);
        s.bg_fill = fill;
        s.weak_bg_fill = fill;
        s.bg_stroke = Stroke::NONE;
        s.expansion = 0.0;
    }
    w.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    w.hovered.fg_stroke = Stroke::new(1.0, Color32::WHITE);
    w.active.fg_stroke = Stroke::new(1.0, Color32::WHITE);
    w.hovered.bg_stroke = Stroke::new(1.0, SURFACE_4);
    v.slider_trailing_fill = true;
    v.striped = false;
    v.handle_shape = egui::style::HandleShape::Rect { aspect_ratio: 0.6 };
    style.visuals = v;
    style.spacing.button_padding = egui::vec2(9.0, 4.0);
    style.spacing.item_spacing = egui::vec2(6.0, 6.0);
    style.spacing.interact_size.y = 22.0;
    style.spacing.menu_margin = egui::Margin::same(4);
    style.spacing.window_margin = egui::Margin::same(0);
    style.spacing.slider_rail_height = 3.0;
    style.spacing.combo_height = 320.0;
    style.text_styles.insert(TextStyle::Body, regular(12.0));
    style.text_styles.insert(TextStyle::Button, regular(12.0));
    style.text_styles.insert(TextStyle::Small, regular(10.5));
    style.text_styles.insert(TextStyle::Heading, semibold(14.0));
    style
        .text_styles
        .insert(TextStyle::Monospace, FontId::monospace(11.5));
    ctx.set_global_style(style);
}
