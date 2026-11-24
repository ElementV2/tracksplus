//! Widgets maison : vumètres professionnels, icônes, boutons, contrôle segmenté.

use super::theme as t;
use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Mesh, Painter, Pos2, Rect, Response, Sense, Shape,
    Stroke, Ui, pos2, vec2,
};

// ------------------------------------------------------------------ vumètres

/// Graduation des vumètres (dB) et leurs libellés.
pub const SCALE: [f32; 11] = [
    0.0, -3.0, -6.0, -9.0, -12.0, -18.0, -24.0, -30.0, -40.0, -50.0, -60.0,
];

/// Déviation d'un vumètre crête façon IEC 60268-18 : plus de résolution près de 0 dB.
pub fn deflection(db: f32) -> f32 {
    let d = if db < -70.0 {
        0.0
    } else if db < -60.0 {
        (db + 70.0) * 0.25
    } else if db < -50.0 {
        (db + 60.0) * 0.5 + 2.5
    } else if db < -40.0 {
        (db + 50.0) * 0.75 + 7.5
    } else if db < -30.0 {
        (db + 40.0) * 1.5 + 15.0
    } else if db < -20.0 {
        (db + 30.0) * 2.0 + 30.0
    } else if db < 0.0 {
        (db + 20.0) * 2.5 + 50.0
    } else {
        100.0
    };
    d / 100.0
}

pub fn to_db(x: f32) -> f32 {
    if x > 1e-7 { 20.0 * x.log10() } else { -140.0 }
}

pub fn fmt_db(db: f32) -> String {
    if db <= -100.0 {
        "−∞".into()
    } else if db > -0.05 && db < 0.05 {
        "0.0".into()
    } else {
        format!("{db:.1}").replace('-', "−")
    }
}

/// État d'affichage d'un vumètre : chute progressive, crête maintenue, saturation.
#[derive(Clone, Copy, Default)]
pub struct MeterState {
    /// Niveau affiché (linéaire).
    pub level: f32,
    /// Crête maintenue (linéaire) et instant de fin du maintien.
    pub hold: f32,
    hold_until: f64,
    /// Crête maximale depuis la dernière remise à zéro (affichage numérique).
    pub max: f32,
    /// Saturation mémorisée (jusqu'au clic).
    pub clip: bool,
}

impl MeterState {
    /// `peak` = crête mesurée depuis la dernière image.
    pub fn update(&mut self, peak: f32, now: f64, dt: f32) {
        // Retour : 13,3 dB/s environ (IEC type II).
        let fall = 10f32.powf(-13.3 * dt / 20.0);
        self.level = peak.max(self.level * fall);
        if peak >= self.hold || now > self.hold_until {
            if peak >= self.hold {
                self.hold = peak;
                self.hold_until = now + 1.8;
            } else {
                self.hold *= 10f32.powf(-20.0 * dt / 20.0);
            }
        }
        self.max = self.max.max(peak);
        if peak >= 0.999 {
            self.clip = true;
        }
    }

    pub fn reset(&mut self) {
        self.max = 0.0;
        self.clip = false;
        self.hold = 0.0;
    }
}

fn zone_color(db: f32) -> Color32 {
    if db >= -3.0 {
        t::METER_RED
    } else if db >= -12.0 {
        t::METER_YELLOW
    } else {
        t::METER_GREEN
    }
}

/// Zones colorées d'un vumètre (de -∞ à 0 dB) : (fraction début, fraction fin, couleur).
fn zones() -> [(f32, f32, Color32); 3] {
    [
        (0.0, deflection(-12.0), t::METER_GREEN),
        (deflection(-12.0), deflection(-3.0), t::METER_YELLOW),
        (deflection(-3.0), 1.0, t::METER_RED),
    ]
}

/// Vumètre horizontal (en-tête de piste).
pub fn meter_h(p: &Painter, rect: Rect, m: &MeterState) {
    p.rect_filled(rect, CornerRadius::same(2), t::METER_OFF);
    let lit = deflection(to_db(m.level));
    let w = rect.width();
    for (a, b, c) in zones() {
        let b2 = b.min(lit);
        if b2 > a {
            let r = Rect::from_min_max(
                pos2(rect.left() + a * w, rect.top()),
                pos2(rect.left() + b2 * w, rect.bottom()),
            );
            p.rect_filled(r, CornerRadius::ZERO, c);
        }
        // Partie éteinte légèrement teintée pour lire les zones.
        let a2 = a.max(lit);
        if b > a2 {
            let r = Rect::from_min_max(
                pos2(rect.left() + a2 * w, rect.top()),
                pos2(rect.left() + b * w, rect.bottom()),
            );
            p.rect_filled(r, CornerRadius::ZERO, c.gamma_multiply(0.07));
        }
    }
    for db in [-48.0f32, -36.0, -24.0, -18.0, -12.0, -6.0, -3.0] {
        let x = rect.left() + deflection(db) * w;
        p.line_segment(
            [pos2(x, rect.top()), pos2(x, rect.bottom())],
            Stroke::new(1.0, Color32::from_black_alpha(110)),
        );
    }
    let hdb = to_db(m.hold);
    if hdb > -70.0 {
        let x = rect.left() + deflection(hdb) * w;
        p.rect_filled(
            Rect::from_min_max(pos2(x - 1.0, rect.top()), pos2(x + 1.0, rect.bottom())),
            0.0,
            zone_color(hdb),
        );
    }
}

/// Vumètre vertical (barre seule).
pub fn meter_v(p: &Painter, rect: Rect, m: &MeterState) {
    p.rect_filled(rect, CornerRadius::same(2), t::METER_OFF);
    let lit = deflection(to_db(m.level));
    let h = rect.height();
    let y = |f: f32| rect.bottom() - f * h;
    for (a, b, c) in zones() {
        let b2 = b.min(lit);
        if b2 > a {
            p.rect_filled(
                Rect::from_min_max(pos2(rect.left(), y(b2)), pos2(rect.right(), y(a))),
                0.0,
                c,
            );
        }
        let a2 = a.max(lit);
        if b > a2 {
            p.rect_filled(
                Rect::from_min_max(pos2(rect.left(), y(b)), pos2(rect.right(), y(a2))),
                0.0,
                c.gamma_multiply(0.07),
            );
        }
    }
    // Segments (lecture plus « hardware »).
    let mut yy = rect.bottom() - 3.0;
    while yy > rect.top() {
        p.line_segment(
            [pos2(rect.left(), yy), pos2(rect.right(), yy)],
            Stroke::new(1.0, Color32::from_black_alpha(70)),
        );
        yy -= 3.0;
    }
    let hdb = to_db(m.hold);
    if hdb > -70.0 {
        let yh = y(deflection(hdb));
        p.rect_filled(
            Rect::from_min_max(pos2(rect.left(), yh - 1.0), pos2(rect.right(), yh + 1.0)),
            0.0,
            zone_color(hdb),
        );
    }
}

/// Graduation verticale en dB à côté d'un vumètre.
pub fn scale_v(p: &Painter, rect: Rect, align_right: bool) {
    let mut last_y = f32::MAX;
    for db in SCALE.iter().rev().copied() {
        let y = rect.bottom() - deflection(db) * rect.height();
        // Évite les libellés qui se chevauchent sur les petits vumètres (0 toujours affiché).
        if last_y - y < 11.0 && db != 0.0 {
            continue;
        }
        last_y = y;
        let (x, anchor) = if align_right {
            (rect.right(), Align2::RIGHT_CENTER)
        } else {
            (rect.left(), Align2::LEFT_CENTER)
        };
        p.text(
            pos2(x, y),
            anchor,
            format!("{}", db.abs() as i32),
            FontId::proportional(9.0),
            t::TEXT_3,
        );
    }
}

/// Voyant de saturation + valeur crête. Retourne la zone cliquable.
pub fn peak_readout(p: &Painter, rect: Rect, m: &MeterState) {
    let db = to_db(m.max);
    let (bg, fg) = if m.clip {
        (t::METER_RED, Color32::WHITE)
    } else {
        (
            t::SURFACE_3,
            if db > -100.0 {
                zone_color(db).gamma_multiply(0.95)
            } else {
                t::TEXT_3
            },
        )
    };
    p.rect_filled(rect, CornerRadius::same(4), bg);
    p.text(
        rect.center(),
        Align2::CENTER_CENTER,
        fmt_db(db),
        t::medium(10.0),
        fg,
    );
}

// ------------------------------------------------------------------ icônes

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    ToStart,
    Stop,
    Play,
    Record,
    Plus,
    Marker,
    Follow,
    Meters,
    Export,
    More,
    Close,
    Speaker,
}

pub fn paint_icon(p: &Painter, icon: Icon, c: Pos2, s: f32, col: Color32) {
    let st = Stroke::new(1.6 * s / 10.0, col);
    match icon {
        Icon::Play => {
            let pts = vec![
                c + vec2(-0.55, -0.75) * s,
                c + vec2(0.85, 0.0) * s,
                c + vec2(-0.55, 0.75) * s,
            ];
            p.add(Shape::convex_polygon(pts, col, Stroke::NONE));
        }
        Icon::Stop => {
            p.rect_filled(
                Rect::from_center_size(c, vec2(1.3, 1.3) * s),
                CornerRadius::same((s * 0.2) as u8),
                col,
            );
        }
        Icon::Record => {
            p.circle_filled(c, 0.75 * s, col);
        }
        Icon::ToStart => {
            p.rect_filled(
                Rect::from_center_size(c + vec2(-0.7, 0.0) * s, vec2(0.22, 1.4) * s),
                CornerRadius::same(1),
                col,
            );
            let pts = vec![
                c + vec2(0.75, -0.7) * s,
                c + vec2(0.75, 0.7) * s,
                c + vec2(-0.45, 0.0) * s,
            ];
            p.add(Shape::convex_polygon(pts, col, Stroke::NONE));
        }
        Icon::Plus => {
            p.line_segment([c + vec2(-0.7, 0.0) * s, c + vec2(0.7, 0.0) * s], st);
            p.line_segment([c + vec2(0.0, -0.7) * s, c + vec2(0.0, 0.7) * s], st);
        }
        Icon::Close => {
            p.line_segment([c + vec2(-0.55, -0.55) * s, c + vec2(0.55, 0.55) * s], st);
            p.line_segment([c + vec2(-0.55, 0.55) * s, c + vec2(0.55, -0.55) * s], st);
        }
        Icon::Marker => {
            p.line_segment([c + vec2(-0.45, -0.8) * s, c + vec2(-0.45, 0.8) * s], st);
            let pts = vec![
                c + vec2(-0.45, -0.8) * s,
                c + vec2(0.65, -0.45) * s,
                c + vec2(-0.45, -0.1) * s,
            ];
            p.add(Shape::convex_polygon(pts, col, Stroke::NONE));
        }
        Icon::Follow => {
            p.line_segment([c + vec2(-0.2, -0.8) * s, c + vec2(-0.2, 0.8) * s], st);
            p.line_segment([c + vec2(0.2, 0.0) * s, c + vec2(0.85, 0.0) * s], st);
            p.line_segment([c + vec2(0.55, -0.3) * s, c + vec2(0.85, 0.0) * s], st);
            p.line_segment([c + vec2(0.55, 0.3) * s, c + vec2(0.85, 0.0) * s], st);
        }
        Icon::Meters => {
            for (i, h) in [0.9f32, 1.4, 0.6, 1.15].iter().enumerate() {
                let x = c.x + (i as f32 - 1.5) * 0.42 * s;
                p.rect_filled(
                    Rect::from_min_max(
                        pos2(x - 0.13 * s, c.y + 0.75 * s - h * s),
                        pos2(x + 0.13 * s, c.y + 0.75 * s),
                    ),
                    CornerRadius::same(1),
                    col,
                );
            }
        }
        Icon::Export => {
            p.line_segment([c + vec2(0.0, 0.35) * s, c + vec2(0.0, -0.8) * s], st);
            p.line_segment([c + vec2(-0.4, -0.4) * s, c + vec2(0.0, -0.8) * s], st);
            p.line_segment([c + vec2(0.4, -0.4) * s, c + vec2(0.0, -0.8) * s], st);
            p.line_segment([c + vec2(-0.75, 0.2) * s, c + vec2(-0.75, 0.8) * s], st);
            p.line_segment([c + vec2(-0.75, 0.8) * s, c + vec2(0.75, 0.8) * s], st);
            p.line_segment([c + vec2(0.75, 0.8) * s, c + vec2(0.75, 0.2) * s], st);
        }
        Icon::More => {
            for k in -1..=1 {
                p.circle_filled(c + vec2(k as f32 * 0.55 * s, 0.0), 0.14 * s, col);
            }
        }
        Icon::Speaker => {
            let pts = vec![
                c + vec2(-0.8, -0.3) * s,
                c + vec2(-0.4, -0.3) * s,
                c + vec2(0.1, -0.75) * s,
                c + vec2(0.1, 0.75) * s,
                c + vec2(-0.4, 0.3) * s,
                c + vec2(-0.8, 0.3) * s,
            ];
            p.add(Shape::convex_polygon(pts, col, Stroke::NONE));
            p.line_segment([c + vec2(0.4, -0.35) * s, c + vec2(0.4, 0.35) * s], st);
            p.line_segment([c + vec2(0.7, -0.6) * s, c + vec2(0.7, 0.6) * s], st);
        }
    }
}

#[derive(Clone, Copy)]
pub enum BtnStyle {
    /// Bouton actif (fond coloré, icône blanche).
    Active(Color32),
    /// Icône colorée sur fond discret.
    Tinted(Color32),
    /// Sans fond.
    Ghost,
}

pub fn icon_button(
    ui: &mut Ui,
    icon: Icon,
    size: Vec2Like,
    style: BtnStyle,
    enabled: bool,
    tip: &str,
) -> Response {
    let sz = size.into_vec();
    let (rect, resp) = ui.allocate_exact_size(
        sz,
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    let resp = if tip.is_empty() {
        resp
    } else {
        resp.on_hover_text(tip)
    };
    let hovered = resp.hovered() && enabled;
    let pressed = resp.is_pointer_button_down_on() && enabled;
    let (bg, fg) = match style {
        BtnStyle::Active(c) => (c, Color32::WHITE),
        BtnStyle::Tinted(c) => (if hovered { t::SURFACE_4 } else { t::SURFACE_3 }, c),
        BtnStyle::Ghost => (
            if hovered {
                t::SURFACE_3
            } else {
                Color32::TRANSPARENT
            },
            t::TEXT_2,
        ),
    };
    let bg = if pressed { bg.gamma_multiply(0.85) } else { bg };
    let fg = if enabled { fg } else { fg.gamma_multiply(0.3) };
    let p = ui.painter();
    let r = (sz.y * 0.28).min(12.0) as u8;
    p.rect_filled(rect, CornerRadius::same(r), bg);
    paint_icon(p, icon, rect.center(), sz.y * 0.24, fg);
    resp
}

/// Taille d'un bouton icône (carré).
pub enum Vec2Like {
    Square(f32),
}

impl Vec2Like {
    fn into_vec(self) -> egui::Vec2 {
        match self {
            Vec2Like::Square(s) => vec2(s, s),
        }
    }
}

impl From<f32> for Vec2Like {
    fn from(s: f32) -> Self {
        Vec2Like::Square(s)
    }
}

pub fn sq(s: f32) -> Vec2Like {
    Vec2Like::Square(s)
}

/// Bouton texte + icône optionnelle, style « pilule ».
pub fn pill(
    ui: &mut Ui,
    icon: Option<Icon>,
    label: &str,
    active: Option<Color32>,
    enabled: bool,
) -> Response {
    let font = t::medium(12.5);
    let galley = ui.painter().layout_no_wrap(label.to_owned(), font, t::TEXT);
    let icon_w = if icon.is_some() { 18.0 } else { 0.0 };
    let w = galley.size().x + icon_w + 24.0;
    let (rect, resp) = ui.allocate_exact_size(
        vec2(w, 30.0),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    let hovered = resp.hovered() && enabled;
    let (bg, fg) = match active {
        Some(c) => (c, Color32::WHITE),
        None => (if hovered { t::SURFACE_4 } else { t::SURFACE_3 }, t::TEXT),
    };
    let fg = if enabled { fg } else { t::TEXT_3 };
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(15), bg);
    let mut x = rect.left() + 12.0;
    if let Some(i) = icon {
        paint_icon(p, i, pos2(x + 6.0, rect.center().y), 6.5, fg);
        x += icon_w;
    }
    p.galley(pos2(x, rect.center().y - galley.size().y / 2.0), galley, fg);
    resp
}

/// Contrôle segmenté (onglets façon macOS). Retourne true si la valeur change.
pub fn segmented(ui: &mut Ui, value: &mut usize, labels: &[&str], seg_w: f32, h: f32) -> bool {
    let n = labels.len();
    let (rect, _) = ui.allocate_exact_size(vec2(seg_w * n as f32 + 6.0, h), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same((h / 2.0) as u8), t::SURFACE_2);
    let mut changed = false;
    for (i, l) in labels.iter().enumerate() {
        let r = Rect::from_min_size(
            pos2(rect.left() + 3.0 + i as f32 * seg_w, rect.top() + 3.0),
            vec2(seg_w, h - 6.0),
        );
        let resp = ui.interact(r, ui.make_persistent_id(("seg", l)), Sense::click());
        if resp.clicked() && *value != i {
            *value = i;
            changed = true;
        }
        let sel = *value == i;
        let p = ui.painter();
        if sel {
            p.rect_filled(r, CornerRadius::same(((h - 6.0) / 2.0) as u8), t::SURFACE_4);
        } else if resp.hovered() {
            p.rect_filled(r, CornerRadius::same(((h - 6.0) / 2.0) as u8), t::SURFACE_3);
        }
        p.text(
            r.center(),
            Align2::CENTER_CENTER,
            *l,
            t::medium(13.0),
            if sel { t::TEXT } else { t::TEXT_2 },
        );
    }
    changed
}

/// Petit bouton carré à libellé (M, S…).
pub fn tag_button(
    ui: &mut Ui,
    label: &str,
    on: bool,
    on_color: Color32,
    enabled: bool,
    tip: &str,
) -> Response {
    let (rect, resp) = ui.allocate_exact_size(
        vec2(22.0, 20.0),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    let resp = resp.on_hover_text(tip);
    let hovered = resp.hovered() && enabled;
    let (bg, fg) = if on {
        (on_color, Color32::BLACK)
    } else {
        (if hovered { t::SURFACE_4 } else { t::SURFACE_3 }, t::TEXT_2)
    };
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(5), bg);
    p.text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        t::semibold(10.5),
        if enabled { fg } else { t::TEXT_3 },
    );
    resp
}

/// Bouton d'armement d'une piste.
pub fn arm_button(ui: &mut Ui, armed: bool, enabled: bool, blink: bool) -> Response {
    let (rect, resp) = ui.allocate_exact_size(
        vec2(22.0, 22.0),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    let resp = resp.on_hover_text("Armer pour l'enregistrement");
    let p = ui.painter();
    let c = rect.center();
    if armed {
        let col = if blink {
            t::REC
        } else {
            t::REC.gamma_multiply(0.75)
        };
        p.circle_filled(c, 10.0, col);
        p.circle_filled(c, 4.0, Color32::WHITE);
    } else {
        let bg = if resp.hovered() && enabled {
            t::SURFACE_4
        } else {
            t::SURFACE_3
        };
        p.circle_filled(c, 10.0, bg);
        p.circle_filled(
            c,
            4.0,
            if enabled {
                t::REC.gamma_multiply(0.8)
            } else {
                t::TEXT_3
            },
        );
    }
    resp
}

/// Texte à chasse fixe (chiffres alignés) avec la police Inter.
pub fn counter(p: &Painter, center: Pos2, text: &str, font: FontId, color: Color32) {
    let digit_w = p.layout_no_wrap("0".into(), font.clone(), color).size().x;
    let sep_w = digit_w * 0.55;
    let width: f32 = text
        .chars()
        .map(|c| if c.is_ascii_digit() { digit_w } else { sep_w })
        .sum();
    let mut x = center.x - width / 2.0;
    for ch in text.chars() {
        let w = if ch.is_ascii_digit() { digit_w } else { sep_w };
        p.text(
            pos2(x + w / 2.0, center.y),
            Align2::CENTER_CENTER,
            ch,
            font.clone(),
            color,
        );
        x += w;
    }
}

/// Rectangle aux coins arrondis en pointillés (pads vides).
pub fn dashed_rect(p: &Painter, rect: Rect, color: Color32) {
    let pts = [
        rect.left_top(),
        rect.right_top(),
        rect.right_bottom(),
        rect.left_bottom(),
        rect.left_top(),
    ];
    p.extend(Shape::dashed_line(&pts, Stroke::new(1.0, color), 5.0, 4.0));
}

/// Forme d'onde min/max dans un mesh (une colonne par pixel).
pub fn wave_mesh(
    rect: Rect,
    color: Color32,
    mut col: impl FnMut(usize) -> Option<(f32, f32)>,
) -> Mesh {
    let mut mesh = Mesh::default();
    let mid = rect.center().y;
    let half = rect.height() * 0.5;
    let n = rect.width().max(0.0) as usize;
    for i in 0..n {
        if let Some((mn, mx)) = col(i) {
            let x = rect.left() + i as f32;
            let top = mid - mx.clamp(-1.0, 1.0) * half;
            let bot = mid - mn.clamp(-1.0, 1.0) * half;
            mesh.add_colored_rect(
                Rect::from_min_max(
                    pos2(x, top.min(mid - 0.5)),
                    pos2(x + 1.0, bot.max(mid + 0.5)),
                ),
                color,
            );
        }
    }
    mesh
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deflection_is_monotonic() {
        let mut last = -1.0;
        for i in 0..=800 {
            let db = -80.0 + i as f32 * 0.1;
            let d = deflection(db);
            assert!(d >= last);
            last = d;
        }
        assert_eq!(deflection(0.0), 1.0);
        assert!((deflection(-20.0) - 0.5).abs() < 1e-6);
        assert_eq!(fmt_db(-6.04), "−6.0");
    }
}
