//! Widgets maison : vumètres, icônes, boutons d'outils, onglets.
//! Style plat et dense : pas de pilules, angles de 2 px, états lisibles au contraste.

use super::theme as t;
use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Mesh, Painter, Pos2, Rect, Response, Sense,
    Stroke, Ui, pos2, vec2,
};

// ------------------------------------------------------------------ vumètres

/// Graduation des vumètres (dB).
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

/// Vitesse de retour des vumètres (dB par seconde).
pub const METER_FALL_DB_S: f32 = 24.0;

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
    /// `dt` = temps réel écoulé depuis la mise à jour précédente (s).
    pub fn update(&mut self, peak: f32, now: f64, dt: f32) {
        // Balistique de crête numérique (consoles, stations audio) : attaque instantanée
        // (crête lue depuis le dernier affichage, rien n'est manqué), retour 24 dB/s.
        let fall = 10f32.powf(-METER_FALL_DB_S * dt / 20.0);
        self.level = peak.max(self.level * fall);
        // Maintien de crête 1,5 s, puis retombée rapide.
        if peak >= self.hold {
            self.hold = peak;
            self.hold_until = now + 1.5;
        } else if now > self.hold_until {
            self.hold *= 10f32.powf(-48.0 * dt / 20.0);
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

    /// Vumètre combiné de plusieurs canaux (affichage numérique d'une piste stéréo).
    pub fn merge(self, o: MeterState) -> MeterState {
        MeterState {
            level: self.level.max(o.level),
            hold: self.hold.max(o.hold),
            hold_until: self.hold_until.max(o.hold_until),
            max: self.max.max(o.max),
            clip: self.clip || o.clip,
        }
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

/// Zones colorées d'un vumètre : (fraction début, fraction fin, couleur).
fn zones() -> [(f32, f32, Color32); 3] {
    [
        (0.0, deflection(-12.0), t::METER_GREEN),
        (deflection(-12.0), deflection(-3.0), t::METER_YELLOW),
        (deflection(-3.0), 1.0, t::METER_RED),
    ]
}

/// Vumètre horizontal (en-tête de piste).
pub fn meter_h(p: &Painter, rect: Rect, m: &MeterState) {
    p.rect_filled(rect, CornerRadius::ZERO, t::METER_OFF);
    let lit = deflection(to_db(m.level));
    let w = rect.width();
    let x = |f: f32| rect.left() + f * w;
    for (a, b, c) in zones() {
        let b2 = b.min(lit);
        if b2 > a {
            p.rect_filled(
                Rect::from_min_max(pos2(x(a), rect.top()), pos2(x(b2), rect.bottom())),
                0.0,
                c,
            );
        }
    }
    for db in [-48.0f32, -24.0, -12.0, -6.0, -3.0] {
        let xx = x(deflection(db)).round() + 0.5;
        p.line_segment(
            [pos2(xx, rect.top()), pos2(xx, rect.bottom())],
            Stroke::new(1.0, Color32::from_black_alpha(140)),
        );
    }
    let hdb = to_db(m.hold);
    if hdb > -70.0 {
        let xx = x(deflection(hdb));
        p.rect_filled(
            Rect::from_min_max(pos2(xx - 1.0, rect.top()), pos2(xx + 1.0, rect.bottom())),
            0.0,
            zone_color(hdb),
        );
    }
}

/// Vumètre vertical (barre seule).
pub fn meter_v(p: &Painter, rect: Rect, m: &MeterState) {
    p.rect_filled(rect, CornerRadius::ZERO, t::METER_OFF);
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
        let y = (rect.bottom() - deflection(db) * rect.height()).round() + 0.5;
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
            t::regular(9.0),
            t::TEXT_3,
        );
    }
}

/// Valeur crête max (dB) ; fond rouge si saturation.
pub fn peak_readout(p: &Painter, rect: Rect, m: &MeterState) {
    let db = to_db(m.max);
    let (bg, fg) = if m.clip {
        (t::METER_RED, Color32::BLACK)
    } else {
        (
            t::INSET,
            if db > -100.0 {
                zone_color(db)
            } else {
                t::TEXT_3
            },
        )
    };
    p.rect_filled(rect, CornerRadius::same(t::R), bg);
    p.text(
        rect.center(),
        Align2::CENTER_CENTER,
        fmt_db(db),
        t::medium(9.5),
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
    Snap,
    Undo,
    /// Casque (monitoring direct).
    Headphones,
    /// Flèche vers le bas sur une ligne (mise à jour, téléchargement).
    Download,
    /// Dossier (ouvrir une session).
    Folder,
    /// Crochet ouvrant « [ » : point d'entrée de la plage.
    MarkIn,
    /// Crochet fermant « ] » : point de sortie de la plage.
    MarkOut,
    /// Ciseaux (couper une prise).
    Scissors,
    /// Corbeille (supprimer).
    Trash,
    /// Rétablir (annuler en miroir).
    Redo,
    /// Plage barrée (effacer la plage de temps).
    ClearRange,
    /// Cadenas (mode spectacle : édition verrouillée).
    Lock,
}

/// Glyphe Phosphor (police d'icônes pro, licence MIT) de chaque icône, et sa graisse :
/// pleine (`true`) pour le transport et les symboles, « Bold » pour les outils.
fn icon_glyph(icon: Icon) -> (&'static str, bool) {
    match icon {
        Icon::ToStart => ("\u{E5A4}", true),         // skip-back
        Icon::Stop => ("\u{E46C}", true),            // stop
        Icon::Play => ("\u{E3D0}", true),            // play
        Icon::Record => ("\u{E18A}", true),          // circle
        Icon::Plus => ("\u{E3D4}", false),           // plus
        Icon::Marker => ("\u{E244}", true),          // flag
        Icon::Follow => ("\u{E064}", false),         // arrow-line-right
        Icon::Meters => ("\u{E228}", false),         // faders
        Icon::Export => ("\u{EAF0}", false),         // export
        Icon::More => ("\u{E1FE}", false),           // dots-three
        Icon::Close => ("\u{E4F6}", false),          // x
        Icon::Speaker => ("\u{E44A}", true),         // speaker-high
        Icon::Snap => ("\u{E680}", false),           // magnet
        Icon::Undo => ("\u{E08A}", false),           // arrow-u-up-left
        Icon::Redo => ("\u{E08C}", false),           // arrow-u-up-right
        Icon::Headphones => ("\u{E2A6}", true),      // headphones
        Icon::Download => ("\u{E20C}", false),       // download-simple
        Icon::Folder => ("\u{E256}", false),         // folder-open
        Icon::MarkIn | Icon::MarkOut => ("", false), // crochets dessinés en texte
        Icon::Scissors => ("\u{EAE0}", false),       // scissors
        Icon::Trash => ("\u{E4A6}", false),          // trash
        Icon::ClearRange => ("\u{E21E}", false),     // eraser
        Icon::Lock => ("\u{E308}", true),            // lock-simple
    }
}

/// Dessine une icône centrée sur `c`. `s` : demi-taille historique des icônes (le glyphe
/// fait environ 2,6 × `s` pixels).
pub fn paint_icon(p: &Painter, icon: Icon, c: Pos2, s: f32, col: Color32) {
    let size = (s * 2.6).max(8.0);
    match icon {
        // Points d'entrée / de sortie de la plage : de vrais crochets, bien gras.
        Icon::MarkIn | Icon::MarkOut => {
            let txt = if icon == Icon::MarkIn { "[" } else { "]" };
            p.text(
                c,
                Align2::CENTER_CENTER,
                txt,
                FontId::new(size * 1.05, egui::FontFamily::Name("semibold".into())),
                col,
            );
        }
        _ => {
            let (glyph, fill) = icon_glyph(icon);
            let fam = if fill { "ph-fill" } else { "ph-bold" };
            p.text(
                c,
                Align2::CENTER_CENTER,
                glyph,
                FontId::new(size, egui::FontFamily::Name(fam.into())),
                col,
            );
        }
    }
}

#[derive(Clone, Copy)]
pub enum BtnStyle {
    /// Bouton actif (fond coloré, icône sombre).
    Active(Color32),
    /// Icône colorée sur fond neutre.
    Tinted(Color32),
    /// Sans fond (fond au survol).
    Ghost,
}

pub fn icon_button(
    ui: &mut Ui,
    icon: Icon,
    size: f32,
    style: BtnStyle,
    enabled: bool,
    tip: &str,
) -> Response {
    let (rect, resp) = ui.allocate_exact_size(
        vec2(size, size),
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
    let (bg, fg) = match style {
        BtnStyle::Active(c) => (c, Color32::BLACK),
        BtnStyle::Tinted(c) => (if hovered { t::SURFACE_4 } else { t::SURFACE_3 }, c),
        BtnStyle::Ghost => (
            if hovered {
                t::SURFACE_3
            } else {
                Color32::TRANSPARENT
            },
            if hovered { t::TEXT } else { t::TEXT_2 },
        ),
    };
    let fg = if enabled { fg } else { fg.gamma_multiply(0.3) };
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(t::R), bg);
    paint_icon(p, icon, rect.center(), size * 0.24, fg);
    resp
}

/// Bouton de barre d'outils : icône + libellé, état activé en orange.
pub fn tool_button(
    ui: &mut Ui,
    icon: Option<Icon>,
    label: &str,
    active: bool,
    enabled: bool,
) -> Response {
    let font = t::medium(11.5);
    let galley = ui.painter().layout_no_wrap(label.to_owned(), font, t::TEXT);
    let icon_w = if icon.is_some() { 16.0 } else { 0.0 };
    let w = galley.size().x + icon_w + if label.is_empty() { 12.0 } else { 18.0 };
    let (rect, resp) = ui.allocate_exact_size(
        vec2(w, 24.0),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    let hovered = resp.hovered() && enabled;
    let bg = if active {
        t::ACCENT_DIM
    } else if hovered {
        t::SURFACE_4
    } else {
        t::SURFACE_3
    };
    let fg = if !enabled {
        t::TEXT_3
    } else if active {
        t::ACCENT
    } else {
        t::TEXT
    };
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(t::R), bg);
    let mut x = rect.left() + if label.is_empty() { 6.0 } else { 9.0 };
    if let Some(i) = icon {
        paint_icon(p, i, pos2(x + 6.0, rect.center().y), 5.5, fg);
        x += icon_w;
    }
    p.galley(pos2(x, rect.center().y - galley.size().y / 2.0), galley, fg);
    resp
}

/// Bouton pictogramme de barre d'outils : même look que `tool_button`, sans texte (la
/// bulle d'aide explique l'action). `active` = interrupteur allumé (fond orange).
/// Séparation fine entre deux groupes de boutons d'outils.
pub fn tool_sep(ui: &mut Ui) {
    let (r, _) = ui.allocate_exact_size(vec2(13.0, 22.0), Sense::hover());
    let x = r.center().x.round() + 0.5;
    ui.painter().line_segment(
        [pos2(x, r.top()), pos2(x, r.bottom())],
        Stroke::new(1.0, t::SURFACE_4),
    );
}

pub fn tool_icon(ui: &mut Ui, icon: Icon, active: bool, enabled: bool) -> Response {
    let (rect, resp) = ui.allocate_exact_size(
        vec2(32.0, 28.0),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    let hovered = resp.hovered() && enabled;
    let bg = if active {
        if hovered {
            t::mix(t::ACCENT_DIM, t::ACCENT, 0.15)
        } else {
            t::ACCENT_DIM
        }
    } else if hovered {
        t::SURFACE_4
    } else {
        t::SURFACE_3
    };
    let fg = if !enabled {
        t::TEXT_3
    } else if active {
        t::ACCENT
    } else {
        t::TEXT
    };
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(t::R), bg);
    paint_icon(p, icon, rect.center(), 6.5, fg);
    resp
}

/// Style des boutons d'action des fenêtres (tous de la même taille).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ActionStyle {
    /// Action principale : fond orange.
    Primary,
    /// Action secondaire (Annuler, Plus tard…) : fond gris.
    Secondary,
    /// Action destructrice (Supprimer, Quitter pendant une prise) : fond rouge.
    Danger,
}

/// Bouton d'action principale (fond orange, texte sombre).
pub fn primary_button(ui: &mut Ui, icon: Option<Icon>, label: &str, enabled: bool) -> Response {
    action_button(ui, icon, label, enabled, ActionStyle::Primary)
}

/// Bouton d'action secondaire : même taille que le bouton principal.
pub fn secondary_button(ui: &mut Ui, label: &str) -> Response {
    action_button(ui, None, label, true, ActionStyle::Secondary)
}

/// Bouton d'action destructrice : même taille que le bouton principal.
pub fn danger_button(ui: &mut Ui, label: &str) -> Response {
    action_button(ui, None, label, true, ActionStyle::Danger)
}

/// Bouton d'action des fenêtres : 26 px de haut, mêmes marges et police pour tous.
pub fn action_button(
    ui: &mut Ui,
    icon: Option<Icon>,
    label: &str,
    enabled: bool,
    style: ActionStyle,
) -> Response {
    let font = t::semibold(11.5);
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font, Color32::BLACK);
    let icon_w = if icon.is_some() { 16.0 } else { 0.0 };
    let w = galley.size().x + icon_w + 22.0;
    let (rect, resp) = ui.allocate_exact_size(
        vec2(w, 26.0),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    let hovered = resp.hovered() && enabled;
    let (base, text) = match style {
        ActionStyle::Primary => (t::ACCENT, Color32::BLACK),
        ActionStyle::Secondary => (t::SURFACE_3, t::TEXT),
        ActionStyle::Danger => (t::REC, Color32::BLACK),
    };
    let bg = if !enabled {
        t::SURFACE_3
    } else if hovered {
        match style {
            ActionStyle::Secondary => t::SURFACE_4,
            _ => t::mix(base, Color32::WHITE, 0.15),
        }
    } else {
        base
    };
    let fg = if enabled { text } else { t::TEXT_3 };
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(t::R), bg);
    let mut x = rect.left() + 11.0;
    if let Some(i) = icon {
        paint_icon(p, i, pos2(x + 6.0, rect.center().y), 5.5, fg);
        x += icon_w;
    }
    p.galley(pos2(x, rect.center().y - galley.size().y / 2.0), galley, fg);
    resp
}

/// Onglets de vue en capitales, soulignés en orange (façon « ARRANGE · MIX »).
/// Position réelle du curseur dans la fenêtre, demandée au système.
/// Pendant un glisser-déposer de fichiers, Windows ne transmet aucun mouvement de souris
/// à la fenêtre : la position connue d'egui est alors périmée.
pub fn os_pointer_pos(ctx: &egui::Context) -> Option<Pos2> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::POINT;
        use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;
        let mut p = POINT { x: 0, y: 0 };
        // SAFETY : simple lecture de la position du curseur dans `p`.
        if unsafe { GetCursorPos(&mut p) } == 0 {
            return None;
        }
        let (ppp, inner) = ctx.input(|i| (i.pixels_per_point, i.viewport().inner_rect));
        let inner = inner?;
        Some(pos2(
            p.x as f32 / ppp - inner.min.x,
            p.y as f32 / ppp - inner.min.y,
        ))
    }
    #[cfg(not(windows))]
    {
        let _ = ctx;
        None
    }
}

/// Largeur occupée par `view_tabs` (pour réserver la place avant de dessiner).
pub fn view_tabs_width(ui: &Ui, labels: &[&str]) -> f32 {
    let w: f32 = labels
        .iter()
        .map(|l| {
            ui.painter()
                .layout_no_wrap(l.to_string(), t::semibold(11.0), t::TEXT)
                .size()
                .x
                + 24.0
        })
        .sum();
    w + 2.0 * labels.len().saturating_sub(1) as f32
}

/// Raccourcit `text` avec « … » pour qu'il tienne dans `max_w` points.
pub fn fit_text(ui: &Ui, text: &str, font: &FontId, max_w: f32) -> String {
    let width = |s: &str| {
        ui.painter()
            .layout_no_wrap(s.to_owned(), font.clone(), Color32::WHITE)
            .size()
            .x
    };
    if width(text) <= max_w {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let s: String = chars[..mid].iter().collect::<String>() + "…";
        if width(&s) <= max_w {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let kept: String = chars[..lo].iter().collect();
    format!("{}…", kept.trim_end())
}

pub fn view_tabs(ui: &mut Ui, value: &mut usize, labels: &[&str]) -> bool {
    view_tabs_resps(ui, value, labels).0
}

/// Comme `view_tabs`, et retourne aussi la zone de chaque onglet (cibles de dépôt).
pub fn view_tabs_resps(ui: &mut Ui, value: &mut usize, labels: &[&str]) -> (bool, Vec<Response>) {
    let mut resps = Vec::new();
    let mut changed = false;
    ui.spacing_mut().item_spacing.x = 2.0;
    let h = ui.available_height();
    for (i, l) in labels.iter().enumerate() {
        let g = ui
            .painter()
            .layout_no_wrap(l.to_string(), t::semibold(11.0), t::TEXT);
        let (rect, resp) = ui.allocate_exact_size(vec2(g.size().x + 24.0, h), Sense::click());
        let sel = *value == i;
        let p = ui.painter();
        if resp.hovered() && !sel {
            p.rect_filled(rect, 0.0, t::SURFACE_2);
        }
        let col = if sel {
            t::TEXT
        } else if resp.hovered() {
            t::TEXT_2
        } else {
            t::TEXT_3
        };
        p.galley(
            pos2(
                rect.center().x - g.size().x / 2.0,
                rect.center().y - g.size().y / 2.0,
            ),
            g,
            col,
        );
        if sel {
            p.rect_filled(
                Rect::from_min_max(
                    pos2(rect.left() + 8.0, rect.bottom() - 2.0),
                    pos2(rect.right() - 8.0, rect.bottom()),
                ),
                0.0,
                t::ACCENT,
            );
        }
        if resp.clicked() && !sel {
            *value = i;
            changed = true;
        }
        resps.push(resp);
    }
    (changed, resps)
}

/// Icône de l'application (la même que celle de la fenêtre et de l'exécutable), chargée
/// une seule fois.
pub fn app_icon(ui: &mut Ui, size: f32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    paint_app_icon(ui, rect);
    resp
}

/// Dessine l'icône de l'application dans `rect`.
pub fn paint_app_icon(ui: &Ui, rect: Rect) {
    let id = egui::Id::unique("tracksplus_app_icon");
    let tex = ui.ctx().data_mut(|d| d.get_temp::<egui::TextureHandle>(id));
    let tex = match tex {
        Some(t) => t,
        None => {
            let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!(
                "../../assets/icon/tracksplus.png"
            )) else {
                return;
            };
            let img = egui::ColorImage::from_rgba_unmultiplied(
                [icon.width as usize, icon.height as usize],
                &icon.rgba,
            );
            let t = ui.ctx().load_texture(
                "tracksplus_app_icon",
                img,
                // Réduite proprement à toutes les tailles (menu, accueil, fenêtres).
                egui::TextureOptions {
                    mipmap_mode: Some(egui::TextureFilter::Linear),
                    ..egui::TextureOptions::LINEAR
                },
            );
            ui.ctx().data_mut(|d| d.insert_temp(id, t.clone()));
            t
        }
    };
    ui.painter().image(
        tex.id(),
        rect,
        Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
        Color32::WHITE,
    );
}

/// Contrôle segmenté plat (choix exclusif court).
pub fn segmented<T: PartialEq + Copy>(
    ui: &mut Ui,
    id: &str,
    value: &mut T,
    options: &[(T, &str)],
) -> bool {
    let opts: Vec<(T, &str, &str)> = options.iter().map(|&(v, l)| (v, l, "")).collect();
    segmented_tips(ui, id, value, &opts)
}

/// Sélecteur segmenté avec une bulle d'aide par option (texte vide : pas de bulle).
pub fn segmented_tips<T: PartialEq + Copy>(
    ui: &mut Ui,
    id: &str,
    value: &mut T,
    options: &[(T, &str, &str)],
) -> bool {
    let mut changed = false;
    let font = t::medium(11.0);
    let widths: Vec<f32> = options
        .iter()
        .map(|(_, l, _)| {
            ui.painter()
                .layout_no_wrap(l.to_string(), font.clone(), t::TEXT)
                .size()
                .x
                + 18.0
        })
        .collect();
    let total: f32 = widths.iter().sum::<f32>() + 4.0;
    let (rect, _) = ui.allocate_exact_size(vec2(total, 24.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(t::R), t::INSET);
    let mut x = rect.left() + 2.0;
    for (k, ((v, l, tip), w)) in options.iter().zip(widths).enumerate() {
        let r = Rect::from_min_size(pos2(x, rect.top() + 2.0), vec2(w, rect.height() - 4.0));
        let mut resp = ui.interact(r, ui.make_persistent_id((id, k)), Sense::click());
        if !tip.is_empty() {
            resp = resp.on_hover_text(*tip);
        }
        let sel = *value == *v;
        let p = ui.painter();
        if sel {
            p.rect_filled(r, CornerRadius::same(t::R), t::SURFACE_4);
        } else if resp.hovered() {
            p.rect_filled(r, CornerRadius::same(t::R), t::SURFACE_2);
        }
        p.text(
            r.center(),
            Align2::CENTER_CENTER,
            *l,
            font.clone(),
            if sel { t::TEXT } else { t::TEXT_2 },
        );
        if resp.clicked() && !sel {
            *value = *v;
            changed = true;
        }
        x += w;
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
        vec2(20.0, 18.0),
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
    p.rect_filled(rect, CornerRadius::same(t::R), bg);
    p.text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        t::semibold(10.0),
        if enabled { fg } else { t::TEXT_3 },
    );
    resp
}

/// Petit bouton carré à icône, même gabarit que `tag_button` (M, S…).
pub fn icon_tag_button(
    ui: &mut Ui,
    icon: Icon,
    on: bool,
    on_color: Color32,
    enabled: bool,
    tip: &str,
) -> Response {
    let (rect, resp) = ui.allocate_exact_size(
        vec2(20.0, 18.0),
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
    p.rect_filled(rect, CornerRadius::same(t::R), bg);
    paint_icon(
        p,
        icon,
        rect.center(),
        6.0,
        if enabled { fg } else { t::TEXT_3 },
    );
    resp
}

/// Bouton d'armement d'une piste (carré, pastille rouge).
pub fn arm_button(ui: &mut Ui, armed: bool, enabled: bool, bright: bool) -> Response {
    let (rect, resp) = ui.allocate_exact_size(
        vec2(20.0, 18.0),
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
        p.rect_filled(
            rect,
            CornerRadius::same(t::R),
            if bright {
                t::REC
            } else {
                t::mix(t::REC, t::SURFACE_3, 0.35)
            },
        );
        p.circle_filled(c, 3.5, Color32::BLACK);
    } else {
        let bg = if resp.hovered() && enabled {
            t::SURFACE_4
        } else {
            t::SURFACE_3
        };
        p.rect_filled(rect, CornerRadius::same(t::R), bg);
        p.circle_filled(
            c,
            3.5,
            if enabled {
                t::mix(t::REC, t::SURFACE_3, 0.25)
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
    let sep_w = digit_w * 0.5;
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

/// Libellé de section en capitales discrètes.
pub fn section(ui: &mut Ui, title: &str) {
    ui.label(
        egui::RichText::new(title.to_uppercase())
            .font(t::caps(10.0))
            .color(t::TEXT_3),
    );
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
    // Un rectangle par colonne au plus : réservé d'un coup (pas de recopies à chaque image).
    mesh.vertices.reserve(n * 4);
    mesh.indices.reserve(n * 6);
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

/// Trait horizontal d'un pixel.
pub fn hline(p: &Painter, x0: f32, x1: f32, y: f32, c: Color32) {
    p.line_segment(
        [pos2(x0, y.round() + 0.5), pos2(x1, y.round() + 0.5)],
        Stroke::new(1.0, c),
    );
}

/// Trait vertical d'un pixel.
pub fn vline(p: &Painter, x: f32, y0: f32, y1: f32, c: Color32) {
    p.line_segment(
        [pos2(x.round() + 0.5, y0), pos2(x.round() + 0.5, y1)],
        Stroke::new(1.0, c),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Retour du vumètre : 24 dB en 1 s, quelle que soit la cadence d'affichage.
    #[test]
    fn meter_fall_independent_of_frame_rate() {
        for fps in [30usize, 60, 144] {
            let mut m = MeterState::default();
            let dt = 1.0 / fps as f32;
            m.update(1.0, 0.0, dt);
            for k in 1..=fps {
                m.update(0.0, k as f64 * dt as f64, dt);
            }
            let db = to_db(m.level);
            assert!((db + 24.0).abs() < 0.1, "{fps} img/s : {db} dB");
            // Maintien de crête encore à 0 dB juste avant 1,5 s.
            assert!((to_db(m.hold)).abs() < 0.01);
        }
    }

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
