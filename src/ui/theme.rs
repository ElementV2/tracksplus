//! Thème : sombre, doux et épuré (inspiration Apple / Resolume), police Inter.

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Shadow, Stroke,
    TextStyle,
};
use std::sync::Arc;

pub const BG: Color32 = Color32::from_rgb(0x0d, 0x0e, 0x10);
pub const SURFACE: Color32 = Color32::from_rgb(0x15, 0x16, 0x19);
pub const SURFACE_2: Color32 = Color32::from_rgb(0x1c, 0x1d, 0x21);
pub const SURFACE_3: Color32 = Color32::from_rgb(0x25, 0x26, 0x2b);
pub const SURFACE_4: Color32 = Color32::from_rgb(0x30, 0x31, 0x37);
pub const BORDER: Color32 = Color32::from_rgb(0x26, 0x27, 0x2c);
pub const LANE_A: Color32 = Color32::from_rgb(0x11, 0x12, 0x14);
pub const LANE_B: Color32 = Color32::from_rgb(0x13, 0x14, 0x17);

pub const TEXT: Color32 = Color32::from_rgb(0xf2, 0xf2, 0xf4);
pub const TEXT_2: Color32 = Color32::from_rgb(0xa1, 0xa1, 0xaa);
pub const TEXT_3: Color32 = Color32::from_rgb(0x63, 0x63, 0x6c);

pub const ACCENT: Color32 = Color32::from_rgb(0x0a, 0x84, 0xff);
pub const REC: Color32 = Color32::from_rgb(0xff, 0x45, 0x3a);
pub const PLAY: Color32 = Color32::from_rgb(0x30, 0xd1, 0x58);
pub const YELLOW: Color32 = Color32::from_rgb(0xff, 0xd6, 0x0a);
pub const ORANGE: Color32 = Color32::from_rgb(0xff, 0x9f, 0x0a);
pub const PLAYHEAD: Color32 = Color32::from_rgb(0xf5, 0xf5, 0xf7);
pub const SELECTION: Color32 = Color32::from_rgba_unmultiplied_const(0x0a, 0x84, 0xff, 0x22);

pub const METER_OFF: Color32 = Color32::from_rgb(0x1a, 0x1b, 0x1f);
pub const METER_GREEN: Color32 = Color32::from_rgb(0x30, 0xd1, 0x58);
pub const METER_YELLOW: Color32 = Color32::from_rgb(0xff, 0xd6, 0x0a);
pub const METER_RED: Color32 = Color32::from_rgb(0xff, 0x45, 0x3a);

pub const PALETTE: [Color32; 10] = [
    Color32::from_rgb(0x0a, 0x84, 0xff),
    Color32::from_rgb(0x30, 0xd1, 0x58),
    Color32::from_rgb(0x5e, 0x5c, 0xe6),
    Color32::from_rgb(0xff, 0x9f, 0x0a),
    Color32::from_rgb(0xff, 0x37, 0x5f),
    Color32::from_rgb(0x64, 0xd2, 0xff),
    Color32::from_rgb(0xbf, 0x5a, 0xf2),
    Color32::from_rgb(0xff, 0xd6, 0x0a),
    Color32::from_rgb(0x63, 0xe6, 0xe2),
    Color32::from_rgb(0xac, 0x8e, 0x68),
];

pub fn color(i: usize) -> Color32 {
    PALETTE[i % PALETTE.len()]
}

pub const R_SMALL: u8 = 6;
pub const R: u8 = 10;
pub const R_LARGE: u8 = 14;

pub fn medium(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("medium".into()))
}

pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("semibold".into()))
}

pub fn regular(size: f32) -> FontId {
    FontId::proportional(size)
}

pub fn window_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(SURFACE_2)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(R_LARGE)
        .inner_margin(egui::Margin::same(20))
        .shadow(Shadow {
            offset: [0, 12],
            blur: 40,
            spread: 0,
            color: Color32::from_black_alpha(140),
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
    let mut style = (*ctx.global_style()).clone();
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = SURFACE_2;
    v.extreme_bg_color = SURFACE;
    v.faint_bg_color = SURFACE_2;
    v.code_bg_color = SURFACE;
    v.override_text_color = Some(TEXT);
    v.selection.bg_fill = ACCENT;
    v.selection.stroke = Stroke::new(1.0, Color32::WHITE);
    v.hyperlink_color = ACCENT;
    v.window_corner_radius = CornerRadius::same(R_LARGE);
    v.window_stroke = Stroke::new(1.0, BORDER);
    v.menu_corner_radius = CornerRadius::same(R);
    v.popup_shadow = Shadow {
        offset: [0, 8],
        blur: 28,
        spread: 0,
        color: Color32::from_black_alpha(120),
    };
    v.window_shadow = v.popup_shadow;
    let w = &mut v.widgets;
    w.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT_2);
    w.noninteractive.corner_radius = CornerRadius::same(R_SMALL);
    for (s, fill) in [
        (&mut w.inactive, SURFACE_3),
        (&mut w.hovered, SURFACE_4),
        (&mut w.active, SURFACE_4),
        (&mut w.open, SURFACE_4),
    ] {
        s.corner_radius = CornerRadius::same(R_SMALL + 2);
        s.bg_fill = fill;
        s.weak_bg_fill = fill;
        s.bg_stroke = Stroke::NONE;
        s.expansion = 0.0;
    }
    w.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    w.hovered.fg_stroke = Stroke::new(1.0, TEXT);
    w.active.fg_stroke = Stroke::new(1.0, TEXT);
    v.slider_trailing_fill = true;
    v.striped = false;
    style.visuals = v;
    style.spacing.button_padding = egui::vec2(12.0, 6.0);
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.interact_size.y = 28.0;
    style.spacing.menu_margin = egui::Margin::same(6);
    style.spacing.window_margin = egui::Margin::same(20);
    style.spacing.slider_rail_height = 4.0;
    style.text_styles.insert(TextStyle::Body, regular(13.5));
    style.text_styles.insert(TextStyle::Button, medium(13.0));
    style.text_styles.insert(TextStyle::Small, regular(11.0));
    style.text_styles.insert(TextStyle::Heading, semibold(18.0));
    style
        .text_styles
        .insert(TextStyle::Monospace, FontId::monospace(12.5));
    ctx.set_global_style(style);
}
