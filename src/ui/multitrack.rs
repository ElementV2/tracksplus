//! Onglet Multipiste : barre d'outils, pistes, timeline, montage, vumètres.

use super::theme as t;
use super::widgets::{self as w, Icon, MeterState};
use crate::app::{App, Transport, short};
use crate::audio;
use crate::project::{Clip, MIN_CLIP};
use crate::render::{PEAK_BLOCK, PEAK_L1, Peaks};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, Key, Mesh, Rect, Sense, Stroke, StrokeKind, UiBuilder,
    pos2, vec2,
};
use std::collections::HashSet;

const HEADER_W: f32 = 284.0;
/// Hauteur du panneau de réglages d'une piste dépliée.
const INSPECTOR_H: f32 = 132.0;
/// Panneau d'une piste audio : seulement la couleur.
const AUDIO_INSPECTOR_H: f32 = 40.0;

/// Colonnes de forme d'onde déjà calculées, par paquets de `WAVE_CHUNK` pixels : (fichier,
/// zoom, décalage, niveau, paquet) → (min, max) par colonne. Le dessin d'une image ne
/// recalcule que ce qui a changé (zoom, prise en cours qui s'allonge).
#[derive(Default)]
pub struct WaveCache {
    map: std::collections::HashMap<(usize, u64, u64, u8, i64), WaveChunk>,
}

struct WaveChunk {
    cols: Vec<Option<(i8, i8)>>,
    /// Toutes les données nécessaires existaient : le paquet ne changera plus.
    complete: bool,
    /// Nombre de points disponibles au calcul (prise en cours : recalcul s'il a grandi).
    len: usize,
}

const WAVE_CHUNK: i64 = 256;

impl WaveCache {
    /// Min / max de la colonne `k` (en pixels depuis le début de la prise).
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    fn column(
        &mut self,
        id: usize,
        lvl: &[crate::render::Peak],
        level: u8,
        per: f64,
        offset: u64,
        spp: f64,
        k: i64,
    ) -> Option<(i8, i8)> {
        let cols = self.chunk(id, lvl, level, per, offset, spp, k.div_euclid(WAVE_CHUNK));
        cols[k.rem_euclid(WAVE_CHUNK) as usize]
    }

    /// Min / max des colonnes du paquet `chunk` (paquets de `WAVE_CHUNK` pixels depuis le
    /// début de la prise) : une seule recherche dans le cache pour tout le paquet.
    #[allow(clippy::too_many_arguments)]
    fn chunk(
        &mut self,
        id: usize,
        lvl: &[crate::render::Peak],
        level: u8,
        per: f64,
        offset: u64,
        spp: f64,
        chunk: i64,
    ) -> &[Option<(i8, i8)>] {
        let key = (id, spp.to_bits(), offset, level, chunk);
        let fresh = self
            .map
            .get(&key)
            .is_some_and(|c| c.complete || c.len == lvl.len());
        if !fresh {
            if self.map.len() > 4096 {
                self.map.clear();
            }
            let mut complete = true;
            let cols = (0..WAVE_CHUNK)
                .map(|j| {
                    let kk = chunk * WAVE_CHUNK + j;
                    if kk < 0 {
                        return None;
                    }
                    let s0 = offset as f64 + kk as f64 * spp;
                    if spp < per {
                        // Zoom plus fin que les points : courbe lissée entre deux points
                        // voisins (au lieu de marches d'escalier).
                        let f = ((s0 + spp * 0.5) / per - 0.5).max(0.0);
                        let i = f as usize;
                        if i + 1 >= lvl.len() {
                            complete = false;
                        }
                        let a = *lvl.get(i)?;
                        let b = *lvl.get(i + 1).unwrap_or(&a);
                        let u = (f - i as f64) as f32;
                        let mix =
                            |x: i8, y: i8| (x as f32 + (y as f32 - x as f32) * u).round() as i8;
                        return Some((mix(a[0], b[0]), mix(a[1], b[1])));
                    }
                    let i0 = (s0 / per) as usize;
                    let want = (((s0 + spp) / per) as usize).max(i0 + 1);
                    if want > lvl.len() {
                        complete = false;
                    }
                    let i1 = want.min(lvl.len());
                    // Niveaux grossiers : le dernier point se complète encore (prise en cours,
                    // calcul en cours) tant que la forme d'onde grandit.
                    if level > 0 && i1 == lvl.len() {
                        complete = false;
                    }
                    if i0 >= i1 {
                        return None;
                    }
                    let (mut mn, mut mx) = (i8::MAX, i8::MIN);
                    for pk in &lvl[i0..i1] {
                        mn = mn.min(pk[0]);
                        mx = mx.max(pk[1]);
                    }
                    Some((mn, mx))
                })
                .collect();
            self.map.insert(
                key,
                WaveChunk {
                    cols,
                    complete,
                    len: lvl.len(),
                },
            );
        }
        &self.map[&key].cols
    }
}

thread_local! {
    static WAVES: std::cell::RefCell<WaveCache> = std::cell::RefCell::new(WaveCache::default());
}

/// Échelle d'affichage de la forme d'onde : relevée pour les passages faibles (une voix à
/// -20 dB occupe un tiers de la hauteur au lieu d'un dixième) ; 0 dB reste en haut.
/// N'affecte que le dessin, jamais le son.
fn wave_scale(v: i8) -> f32 {
    // Les points sont déjà à l'échelle d'affichage (voir `render::to_peak`).
    v as f32 / 127.0
}

/// Positions possibles du début d'une prise rognée par la gauche : jusqu'au début du
/// fichier, et jamais plus courte que `MIN_CLIP`. Une prise déjà plus courte (clic importé,
/// segment de quelques trames) peut seulement être rallongée : jamais `min > max`, qui
/// ferait paniquer `clamp`.
fn trim_left_range(c: &Clip) -> (i64, i64) {
    let lo = (c.start as i64 - c.offset as i64).max(0);
    let hi = (c.end() as i64 - MIN_CLIP as i64).max(c.start as i64);
    (lo.min(hi), hi)
}

/// Positions possibles de la fin d'une prise rognée par la droite (même règle).
fn trim_right_range(c: &Clip) -> (i64, i64) {
    let hi = c.start as i64 + c.src_end().saturating_sub(c.offset).max(c.len) as i64;
    let lo = (c.start as i64 + MIN_CLIP as i64).min(c.end() as i64);
    (lo.min(hi), hi)
}

/// Couleur d'une piste : celle choisie, sinon selon sa position.
pub fn track_color(track: &crate::project::Track, idx: usize) -> Color32 {
    track
        .color
        .map_or(t::color(idx), |c| t::PALETTE[c as usize % t::PALETTE.len()])
}
const RULER_H: f32 = 24.0;
const SCROLLBAR_H: f32 = 12.0;
const CLIP_HEADER: f32 = 13.0;
const EDGE_GRAB: f32 = 6.0;
const SNAP_PX: f32 = 8.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DragMode {
    Move,
    TrimLeft,
    TrimRight,
    FadeIn,
    FadeOut,
}

/// Glisser-déposer d'une ou plusieurs prises.
pub struct ClipDrag {
    pub mode: DragMode,
    /// Échantillon sous le pointeur au début du geste.
    pub anchor: u64,
    /// Prise saisie.
    pub main: u64,
    /// État d'origine des prises déplacées : (piste, prise).
    pub origs: Vec<(u64, Clip)>,
    /// Piste visée (déplacement vertical d'une seule prise).
    pub target: Option<usize>,
    pub changed: bool,
}

/// Largeur du panneau de vumètre master (multipiste et soundboard).
pub const MASTER_W: f32 = 100.0;

/// Panneau de vumètre master stéréo (partagé avec le soundboard) : graduations à gauche,
/// deux larges barres L / R séparées par un trait fin, crêtes max au-dessus.
/// Vumètre d'une piste : niveau de lecture (après le fader) ou de son entrée. En lecture,
/// toute piste qui a des prises montre ce qu'on entend, même armée ; en prise, une piste
/// armée montre l'entrée enregistrée ; à l'arrêt, une piste d'enregistrement montre son
/// entrée (réglage des niveaux). Une piste audio montre toujours sa lecture.
fn shows_playback(tr: &crate::project::Track, transport: Transport) -> bool {
    match transport {
        _ if tr.is_audio() => true,
        Transport::Playing { .. } => !tr.armed || !tr.clips.is_empty(),
        Transport::Recording { .. } => !tr.armed,
        Transport::Stopped => false,
    }
}

pub fn master_panel(ui: &mut egui::Ui, title: &str, meters: &mut [MeterState; 2]) {
    let full = ui.max_rect();
    let p = ui.painter().clone();
    w::vline(&p, full.left(), full.top(), full.bottom(), t::LINE);
    p.text(
        pos2(full.center().x, full.top() + 13.0),
        Align2::CENTER_CENTER,
        title,
        t::caps(10.0),
        t::TEXT_3,
    );
    // Colonnes : graduations | barre L | séparation | barre R.
    let bw = 26.0;
    let sep = 5.0;
    let rx_end = full.right() - 9.0;
    let rx = rx_end - bw;
    let lx = rx - sep - bw;
    let scale_right = lx - 7.0;

    // Crêtes max au-dessus de chaque barre (clic : remise à zéro).
    let top = full.top() + 26.0;
    let mut reset = false;
    for (k, m) in meters.iter().enumerate() {
        let cx = if k == 0 { lx } else { rx } + bw / 2.0;
        let r = Rect::from_center_size(pos2(cx, top + 8.0), vec2(bw + 4.0, 16.0));
        w::peak_readout(&p, r, m);
        if ui
            .interact(r, ui.make_persistent_id((title, "pk", k)), Sense::click())
            .on_hover_text("Crête — clic : remise à zéro")
            .clicked()
        {
            reset = true;
        }
    }
    if reset {
        for m in meters.iter_mut() {
            m.reset();
        }
    }

    let mtop = top + 26.0;
    let mbot = full.bottom() - 22.0;
    w::meter_v(
        &p,
        Rect::from_min_max(pos2(lx, mtop), pos2(lx + bw, mbot)),
        &meters[0],
    );
    w::meter_v(
        &p,
        Rect::from_min_max(pos2(rx, mtop), pos2(rx + bw, mbot)),
        &meters[1],
    );
    // Trait fin entre les deux barres.
    w::vline(&p, (lx + bw + sep / 2.0).round(), mtop, mbot, t::SURFACE_4);
    // Graduations à gauche : chiffres alignés à droite, petits traits jusqu'à la barre L.
    let h = mbot - mtop;
    let mut last = f32::MAX;
    for db in w::SCALE.iter().rev() {
        let y = mbot - w::deflection(*db) * h;
        w::hline(&p, lx - 4.0, lx - 1.0, y, t::TEXT_3);
        if last - y < 11.0 && *db != 0.0 {
            continue;
        }
        last = y;
        p.text(
            pos2(scale_right, y),
            Align2::RIGHT_CENTER,
            format!("{}", db.abs() as i32),
            t::regular(8.5),
            t::TEXT_3,
        );
    }
    p.text(
        pos2(lx + bw / 2.0, mbot + 10.0),
        Align2::CENTER_CENTER,
        "L",
        t::medium(9.5),
        t::TEXT_3,
    );
    p.text(
        pos2(rx + bw / 2.0, mbot + 10.0),
        Align2::CENTER_CENTER,
        "R",
        t::medium(9.5),
        t::TEXT_3,
    );
}

impl App {
    pub fn multitrack_ui(&mut self, ui: &mut egui::Ui) {
        self.multitrack_keys(ui.ctx());
        egui::Panel::top("mt_tools")
            .resizable(false)
            .exact_size(36.0)
            .frame(
                egui::Frame::new()
                    .fill(t::SURFACE_2)
                    .inner_margin(egui::Margin::symmetric(10, 0)),
            )
            .show(ui, |ui| self.toolbar(ui));
        egui::Panel::right("master")
            .resizable(false)
            .exact_size(MASTER_W)
            .frame(egui::Frame::new().fill(t::SURFACE))
            .show(ui, |ui| {
                let mut m = self.master_play;
                master_panel(ui, "MASTER", &mut m);
                self.master_play = m;
            });
        if self.show_bridge {
            egui::Panel::bottom("bridge")
                .resizable(false)
                .exact_size(250.0)
                .frame(egui::Frame::new().fill(t::SURFACE))
                .show(ui, |ui| self.meter_bridge(ui));
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(t::BG))
            .show(ui, |ui| self.tracks_area(ui));
    }

    fn multitrack_keys(&mut self, ctx: &egui::Context) {
        if self.dialog_open() || ctx.egui_wants_keyboard_input() {
            return;
        }
        let none = egui::Modifiers::NONE;
        let (mark_in, mark_out) =
            ctx.input_mut(|i| (i.consume_key(none, Key::I), i.consume_key(none, Key::O)));
        if mark_in {
            self.set_range_point(self.playhead, true);
        }
        if mark_out {
            self.set_range_point(self.playhead, false);
        }
        if ctx.input_mut(|i| i.consume_key(none, Key::L)) {
            self.toggle_loop();
        }
        let (space, rec, home, end, marker, fit, esc, all, left, right, split, del) = ctx
            .input_mut(|i| {
                (
                    i.consume_key(none, Key::Space),
                    i.consume_key(none, Key::R),
                    i.consume_key(none, Key::Home) || i.consume_key(none, Key::Enter),
                    i.consume_key(none, Key::End),
                    i.consume_key(none, Key::M),
                    i.consume_key(none, Key::F),
                    i.consume_key(none, Key::Escape),
                    i.consume_key(egui::Modifiers::COMMAND, Key::A),
                    i.consume_key(none, Key::ArrowLeft),
                    i.consume_key(none, Key::ArrowRight),
                    i.consume_key(none, Key::S),
                    i.consume_key(none, Key::Delete) || i.consume_key(none, Key::Backspace),
                )
            });
        // Pendant un enregistrement, le clavier ne peut pas l'arrêter d'un seul appui :
        // il faut confirmer (deuxième appui) pour éviter une coupure accidentelle.
        if space || rec {
            if self.is_recording() {
                self.request_stop_recording();
            } else if space {
                if self.is_stopped() {
                    self.play();
                } else {
                    self.stop();
                }
            } else {
                self.record();
            }
        }
        if home {
            self.seek(self.selection.map_or(0, |s| s.0));
            self.view.scroll = 0.0f64.max(self.playhead as f64 - 50.0 * self.view.spp);
        }
        if end && !self.is_recording() {
            self.seek(self.project.end());
        }
        if marker {
            self.add_marker();
        }
        if fit {
            self.zoom_to_fit();
        }
        if esc {
            // Échap annule aussi une relance automatique de prise en attente.
            self.cancel_resume();
            self.selection = None;
            self.ruler_anchor = None;
            self.selected_tracks.clear();
            self.sel_clips.clear();
        }
        if all {
            self.sel_clips = self
                .project
                .tracks
                .iter()
                .flat_map(|t| t.clips.iter().map(|c| c.id))
                .collect();
        }
        if split {
            self.split_at_playhead();
        }
        if del {
            self.delete_selection();
        }
        // Touches 1 à 9 : aller au repère 1 à 9 (dans l'ordre de la timeline).
        let digit = ctx.input_mut(|i| {
            [
                Key::Num1,
                Key::Num2,
                Key::Num3,
                Key::Num4,
                Key::Num5,
                Key::Num6,
                Key::Num7,
                Key::Num8,
                Key::Num9,
            ]
            .iter()
            .position(|&k| i.consume_key(none, k))
        });
        if let Some(n) = digit
            && !self.is_recording()
        {
            match self.project.markers.get(n).map(|m| m.pos) {
                Some(p) => self.seek(p),
                None => self.notify(format!("Pas de repère {}", n + 1), false),
            }
        }
        if (left || right) && !self.is_recording() {
            let ph = self.playhead;
            let target = if left {
                self.project
                    .markers
                    .iter()
                    .rev()
                    .map(|m| m.pos)
                    .find(|&p| p + 2400 < ph)
                    .or(Some(0))
            } else {
                self.project
                    .markers
                    .iter()
                    .map(|m| m.pos)
                    .find(|&p| p > ph + 2400)
            };
            if let Some(p) = target {
                self.seek(p);
            }
        }
    }

    // ---------------------------------------------------------------- barre d'outils

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        let full = ui.max_rect();
        w::hline(
            ui.painter(),
            full.left() - 10.0,
            full.right() + 10.0,
            full.bottom() - 1.0,
            t::LINE,
        );
        let stopped = self.is_stopped();
        // Boutons de montage grisés pendant une prise et sa finalisation.
        let recording = self.editing_locked();
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            // Pictogrammes : la bulle d'aide nomme l'action et son raccourci.
            if w::tool_icon(ui, Icon::Follow, self.view.follow, true)
                .on_hover_text("Suivre la lecture")
                .clicked()
            {
                self.view.follow = !self.view.follow;
                self.view.nav_until = 0.0;
            }
            if w::tool_icon(ui, Icon::Snap, self.snap, true)
                .on_hover_text("Magnétisme · Maj : désactiver")
                .clicked()
            {
                self.snap = !self.snap;
            }
            w::tool_sep(ui);
            if w::tool_icon(ui, Icon::Marker, false, true)
                .on_hover_text("Repère (M)")
                .clicked()
            {
                self.add_marker();
            }
            if w::tool_icon(ui, Icon::MarkIn, false, true)
                .on_hover_text("Début de plage (I)")
                .clicked()
            {
                self.set_range_point(self.playhead, true);
            }
            if w::tool_icon(ui, Icon::MarkOut, false, true)
                .on_hover_text("Fin de plage (O)")
                .clicked()
            {
                self.set_range_point(self.playhead, false);
            }
            let has_range = self.selection.is_some() || self.ruler_anchor.is_some();
            if w::tool_icon(ui, Icon::ClearRange, false, has_range)
                .on_hover_text("Effacer la plage (Échap)")
                .clicked()
            {
                self.selection = None;
                self.ruler_anchor = None;
            }
            w::tool_sep(ui);
            if w::tool_icon(ui, Icon::Scissors, false, !recording)
                .on_hover_text("Couper (S)")
                .clicked()
            {
                self.split_at_playhead();
            }
            let can_del = !recording && (!self.sel_clips.is_empty() || self.selection.is_some());
            if w::tool_icon(ui, Icon::Trash, false, can_del)
                .on_hover_text("Supprimer (Suppr)")
                .clicked()
            {
                self.delete_selection();
            }
            w::tool_sep(ui);
            if w::tool_icon(ui, Icon::Undo, false, !self.undo.is_empty() && !recording)
                .on_hover_text("Annuler (Ctrl+Z)")
                .clicked()
            {
                self.undo();
            }
            if w::tool_icon(ui, Icon::Redo, false, !self.redo.is_empty() && !recording)
                .on_hover_text("Rétablir (Ctrl+Maj+Z)")
                .clicked()
            {
                self.redo();
            }
            w::tool_sep(ui);
            if w::tool_icon(ui, Icon::Meters, self.show_bridge, true)
                .on_hover_text("Mixage et vumètres")
                .clicked()
            {
                self.show_bridge = !self.show_bridge;
            }
        });

        let right = Rect::from_min_max(pos2(full.right() - 420.0, full.top()), full.max);
        let mut child = ui.new_child(
            UiBuilder::new()
                .max_rect(right)
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
        );
        child.spacing_mut().item_spacing.x = 3.0;
        if w::tool_icon(
            &mut child,
            Icon::Export,
            false,
            self.project.has_audio() && stopped,
        )
        .on_hover_text("Exporter (Ctrl+E)")
        .clicked()
        {
            self.open_export();
        }
    }

    /// Défilement affiché, calé au pixel entier : les formes d'onde sont dessinées en
    /// colonnes de pixels ; avec un décalage fractionnaire, elles sautaient d'un pixel par
    /// rapport à leur clip pendant le suivi de lecture (effet de tremblement). Calée ainsi,
    /// toute la timeline avance d'un même nombre de pixels à chaque image.
    fn draw_scroll(&self) -> f64 {
        (self.view.scroll / self.view.spp).round() * self.view.spp
    }

    fn x_of(&self, left: f32, sample: u64) -> f32 {
        left + ((sample as f64 - self.draw_scroll()) / self.view.spp) as f32
    }

    fn sample_at(&self, left: f32, x: f32) -> u64 {
        (self.draw_scroll() + (x - left) as f64 * self.view.spp).max(0.0) as u64
    }

    fn follow_playhead(&mut self) {
        if self.is_stopped()
            || !self.view.follow
            || self.now < self.view.nav_until
            || self.clip_drag.is_some()
        {
            return;
        }
        let visible = self.view.width as f64 * self.view.spp;
        let ph = self.playhead as f64;
        // Défilement continu : la tête reste vers 75 % de la largeur.
        if ph > self.view.scroll + visible * 0.75 {
            self.view.scroll = ph - visible * 0.75;
        } else if ph < self.view.scroll {
            self.view.scroll = (ph - visible * 0.1).max(0.0);
        }
        if self.is_recording() {
            self.reveal_recording_tracks();
        }
    }

    fn user_navigated(&mut self) {
        if !self.is_stopped() {
            self.view.nav_until = self.now + 3.0;
        }
    }

    fn timeline_input(&mut self, ui: &egui::Ui, rect: Rect) {
        if !ui.rect_contains_pointer(rect) || self.dialog_open() {
            return;
        }
        let (zoom, scroll_x, pointer) = ui.input(|i| {
            (
                i.zoom_delta(),
                i.smooth_scroll_delta().x,
                i.pointer.hover_pos(),
            )
        });
        if zoom != 1.0 {
            let px = pointer.map_or(rect.center().x, |p| p.x);
            let anchor = self.view.scroll + (px - rect.left()) as f64 * self.view.spp;
            self.view.spp = (self.view.spp / zoom as f64).clamp(2.0, 1.0e6);
            self.view.scroll = (anchor - (px - rect.left()) as f64 * self.view.spp).max(0.0);
            self.user_navigated();
        }
        if scroll_x != 0.0 {
            self.view.scroll = (self.view.scroll - scroll_x as f64 * self.view.spp).max(0.0);
            self.user_navigated();
        }
    }

    fn ruler_step(&self) -> f64 {
        let px_per_sec = self.sr() as f64 / self.view.spp;
        // Jusqu'à la milliseconde : plus on zoome, plus la grille est fine.
        let steps = [
            0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0,
            30.0, 60.0, 120.0, 300.0, 600.0, 900.0, 1800.0, 3600.0,
        ];
        steps
            .iter()
            .copied()
            .find(|s| s * px_per_sec >= 90.0)
            .unwrap_or(3600.0)
    }

    fn ruler(&mut self, ui: &mut egui::Ui, rect: Rect) {
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 0.0, t::SURFACE_2);
        w::hline(&p, rect.left(), rect.right(), rect.bottom() - 1.0, t::LINE);
        let sr = self.sr() as f64;
        let px_per_sec = sr / self.view.spp;
        let step = self.ruler_step();
        let t0 = self.view.scroll / sr;
        let t1 = t0 + rect.width() as f64 * self.view.spp / sr;
        let mut tt = (t0 / step).floor() * step;
        while tt <= t1 {
            let x = self.x_of(rect.left(), (tt * sr) as u64);
            w::vline(&p, x, rect.bottom() - 9.0, rect.bottom(), t::TEXT_3);
            for k in 1..4 {
                let xs = x + (k as f64 * step / 4.0 * px_per_sec) as f32;
                w::vline(&p, xs, rect.bottom() - 4.0, rect.bottom(), t::SURFACE_4);
            }
            p.text(
                pos2(x + 4.0, rect.top() + 4.0),
                Align2::LEFT_TOP,
                fmt_ruler(tt, step),
                t::regular(10.0),
                t::TEXT_2,
            );
            tt += step;
        }
        if let Some((a, b)) = self.selection {
            let xa = self.x_of(rect.left(), a).max(rect.left());
            let xb = self.x_of(rect.left(), b).min(rect.right());
            if xb > xa {
                p.rect_filled(
                    Rect::from_min_max(pos2(xa, rect.top()), pos2(xb, rect.bottom() - 1.0)),
                    0.0,
                    t::ACCENT.gamma_multiply(0.28),
                );
                // Boucle active : barre pleine en haut de la règle.
                if self.loop_on {
                    p.rect_filled(
                        Rect::from_min_max(pos2(xa, rect.top()), pos2(xb, rect.top() + 4.0)),
                        0.0,
                        t::ACCENT,
                    );
                }
            }
        }
        // Point d'entrée ou de sortie posé seul (Ctrl / Alt + clic), en attente de l'autre.
        if self.selection.is_none()
            && let Some((x, is_in)) = self.ruler_anchor
        {
            let x = self.x_of(rect.left(), x);
            if x >= rect.left() && x <= rect.right() {
                w::vline(&p, x, rect.top(), rect.bottom(), t::ACCENT);
                let tri = if is_in {
                    vec![
                        pos2(x, rect.top()),
                        pos2(x + 7.0, rect.top()),
                        pos2(x, rect.top() + 7.0),
                    ]
                } else {
                    vec![
                        pos2(x, rect.top()),
                        pos2(x - 7.0, rect.top()),
                        pos2(x, rect.top() + 7.0),
                    ]
                };
                p.add(egui::Shape::convex_polygon(tri, t::ACCENT, Stroke::NONE));
            }
        }

        // Étiquettes visibles des repères (une zone d'interaction chacune, à chaque image :
        // indispensable pour que leur menu reste ouvert quand la souris le rejoint).
        let mut marker_rects: Vec<(u64, Rect)> = Vec::new();
        for m in &self.project.markers {
            let x = self.x_of(rect.left(), m.pos);
            if x < rect.left() - 120.0 || x > rect.right() {
                continue;
            }
            let editing = self.marker_editing == Some(m.id);
            let gal = p.layout_no_wrap(m.name.clone(), t::medium(9.5), Color32::BLACK);
            let r = Rect::from_min_size(pos2(x, rect.top() + 2.0), vec2(gal.size().x + 8.0, 13.0));
            let hovered = ui.rect_contains_pointer(r);
            let bg = if editing || hovered {
                t::mix(t::MARKER, Color32::WHITE, 0.3)
            } else {
                t::MARKER
            };
            p.rect_filled(r, CornerRadius::ZERO, bg);
            p.galley(
                pos2(r.left() + 4.0, r.center().y - gal.size().y / 2.0),
                gal,
                Color32::BLACK,
            );
            w::vline(&p, x, rect.top() + 2.0, rect.bottom(), t::MARKER);
            marker_rects.push((m.id, r));
        }

        let x = self.x_of(rect.left(), self.playhead);
        if rect.x_range().contains(x) {
            let tri = vec![
                pos2(x - 5.0, rect.bottom() - 8.0),
                pos2(x + 5.0, rect.bottom() - 8.0),
                pos2(x, rect.bottom()),
            ];
            p.add(egui::Shape::convex_polygon(
                tri,
                if self.is_recording() {
                    t::REC
                } else {
                    t::PLAYHEAD
                },
                Stroke::NONE,
            ));
        }

        let resp = ui.interact(
            rect,
            ui.make_persistent_id("ruler"),
            Sense::click_and_drag(),
        );
        let mut on_marker = false;
        let mut any_menu = false;
        for (id, r) in marker_rects {
            let mresp = ui
                .interact(r, ui.make_persistent_id(("marker", id)), Sense::click())
                .on_hover_text(
                    "Clic : aller au repère · double-clic ou clic droit : renommer, supprimer",
                );
            on_marker |= mresp.hovered() || mresp.is_pointer_button_down_on();
            if mresp.clicked()
                && !mresp.double_clicked()
                && let Some(m) = self.project.markers.iter().find(|m| m.id == id)
            {
                let pos = m.pos;
                self.seek(pos);
            }
            // Menu sous l'étiquette, ouvert par clic droit ou double-clic. (Le menu
            // contextuel d'egui se referme à chaque clic simple : incompatible avec le
            // double-clic.) Il reste ouvert pendant la saisie du nom.
            let open = (mresp.secondary_clicked() || mresp.double_clicked())
                .then_some(egui::SetOpenCommand::Bool(true));
            if egui::Popup::menu(&mresp)
                .open_memory(open)
                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                .show(|ui| self.marker_menu(ui, id))
                .is_some()
            {
                any_menu = true;
            }
        }
        if !any_menu {
            // Menu refermé : la prochaine ouverture redonnera le focus au nom.
            self.marker_editing = None;
        }
        if !on_marker {
            self.time_select(&resp, rect.left());
        }
    }

    fn marker_menu(&mut self, ui: &mut egui::Ui, id: u64) {
        let Some(idx) = self.project.markers.iter().position(|m| m.id == id) else {
            return;
        };
        ui.set_width(220.0);
        w::section(ui, "Repère");
        let name = ui.add(
            egui::TextEdit::singleline(&mut self.project.markers[idx].name)
                .desired_width(200.0)
                .hint_text("Nom du repère"),
        );
        if self.marker_editing != Some(id) {
            // Première image du menu : le nom est prêt à être retapé.
            self.marker_editing = Some(id);
            name.request_focus();
            if let Some(mut st) = egui::TextEdit::load_state(ui.ctx(), name.id) {
                let len = self.project.markers[idx].name.chars().count();
                st.cursor.set_char_range(Some(egui::text::CCursorRange::two(
                    egui::text::CCursor::new(0),
                    egui::text::CCursor::new(len),
                )));
                st.store(ui.ctx(), name.id);
            }
        }
        if name.changed() {
            self.project.dirty = true;
        }
        if name.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            ui.close();
        }
        let pos = self.project.markers[idx].pos;
        let next = self
            .project
            .markers
            .get(idx + 1)
            .map(|m| m.pos)
            .unwrap_or(self.project.end());
        if next > pos && ui.button("Sélectionner jusqu'au repère suivant").clicked() {
            self.selection = Some((pos, next));
            ui.close();
        }
        if ui.button("Supprimer le repère").clicked() {
            self.checkpoint();
            self.project.markers.remove(idx);
            self.project.dirty = true;
            ui.close();
        }
    }

    /// Règle : clic = aller à cet endroit ; glisser = faire défiler la timeline (attraper la
    /// tête de lecture elle-même la déplace). Plage de temps : **Ctrl + clic = point
    /// d'entrée, Alt + clic = point de sortie**. Tout est aimanté à la grille (Maj : libre).
    fn time_select(&mut self, resp: &egui::Response, left: f32) {
        let (free, ctrl, alt) = resp
            .ctx
            .input(|i| (i.modifiers.shift, i.modifiers.command, i.modifiers.alt));
        let ph_x = self.x_of(left, self.playhead);
        let near_head = |x: f32| (x - ph_x).abs() <= 7.0;
        if (self.playhead_drag || resp.hover_pos().is_some_and(|h| near_head(h.x)))
            && !self.is_recording()
        {
            resp.ctx.set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        }
        if resp.drag_started() && !ctrl && !alt {
            let origin = resp.ctx.input(|i| i.pointer.press_origin());
            if let Some(p) = origin.or(resp.interact_pointer_pos())
                && near_head(p.x)
                && !self.is_recording()
            {
                self.playhead_drag = true;
            }
        }
        // Glisser ailleurs sur la règle : défilement horizontal de la timeline.
        if resp.dragged() && !self.playhead_drag && !ctrl && !alt {
            let dx = resp.drag_delta().x as f64;
            if dx != 0.0 {
                self.view.scroll = (self.view.scroll - dx * self.view.spp).max(0.0);
                self.user_navigated();
            }
            resp.ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
        }
        if resp.dragged()
            && self.playhead_drag
            && let Some(p) = resp.interact_pointer_pos()
        {
            let s = self.snap_pos(self.sample_at(left, p.x), free, true);
            self.scrub_to(s);
        }
        if resp.drag_stopped() && self.playhead_drag {
            self.playhead_drag = false;
            // Position finale exacte, aussi en lecture.
            if let Some(p) = resp.interact_pointer_pos() {
                let s = self.snap_pos(self.sample_at(left, p.x), free, true);
                self.seek(s);
            }
        }
        if !resp.clicked() {
            return;
        }
        let Some(p) = resp.interact_pointer_pos() else {
            return;
        };
        let s = self.snap_pos(self.sample_at(left, p.x), free, !(ctrl || alt));
        if ctrl || alt {
            self.set_range_point(s, ctrl);
        } else if !self.is_recording() {
            self.seek(s);
        }
    }

    /// Pose le point d'entrée (`is_in`) ou de sortie de la plage de temps. La plage est
    /// formée dès que l'autre point existe du bon côté ; sinon le point attend l'autre.
    fn set_range_point(&mut self, s: u64, is_in: bool) {
        let other = match (self.selection, self.ruler_anchor) {
            (Some((a, b)), _) => Some(if is_in { b } else { a }),
            (None, Some((x, anchor_in))) if anchor_in != is_in => Some(x),
            _ => None,
        };
        let range = other.and_then(|o| {
            let (a, b) = if is_in { (s, o) } else { (o, s) };
            (b > a).then_some((a, b))
        });
        match range {
            Some(r) => {
                self.selection = Some(r);
                self.ruler_anchor = None;
                if self.is_stopped() {
                    self.playhead = r.0;
                }
            }
            None => {
                self.selection = None;
                self.ruler_anchor = Some((s, is_in));
            }
        }
    }

    fn scrollbar(&mut self, ui: &mut egui::Ui, rect: Rect) {
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 0.0, t::SURFACE);
        let visible = rect.width() as f64 * self.view.spp;
        let total = (self.project.end().max(self.playhead) as f64 + visible * 0.5)
            .max(self.view.scroll + visible);
        let track = rect.shrink2(vec2(2.0, 3.0));
        let x0 = track.left() + (self.view.scroll / total) as f32 * track.width();
        let tw = ((visible / total) as f32 * track.width()).max(30.0);
        let thumb = Rect::from_min_size(pos2(x0, track.top()), vec2(tw, track.height()));
        let resp = ui.interact(
            rect,
            ui.make_persistent_id("hscroll"),
            Sense::click_and_drag(),
        );
        let col = if resp.hovered() || resp.dragged() {
            t::SURFACE_4
        } else {
            t::SURFACE_3
        };
        p.rect_filled(thumb, t::R, col);
        if resp.dragged() {
            let dx = resp.drag_delta().x as f64;
            self.view.scroll = (self.view.scroll + dx / track.width() as f64 * total).max(0.0);
            self.user_navigated();
        } else if resp.clicked()
            && let Some(pp) = resp.interact_pointer_pos()
        {
            let f = ((pp.x - track.left()) / track.width()) as f64;
            self.view.scroll = (f * total - visible / 2.0).max(0.0);
            self.user_navigated();
        }
    }

    // ---------------------------------------------------------------- montage

    /// Points d'aimantation : tête de lecture, repères, sélection, bords des autres prises.
    fn snap_points(&self, exclude: &HashSet<u64>) -> Vec<u64> {
        let mut v = vec![0, self.playhead];
        v.extend(self.project.markers.iter().map(|m| m.pos));
        if let Some((a, b)) = self.selection {
            v.push(a);
            v.push(b);
        }
        for tr in &self.project.tracks {
            for c in tr.clips.iter().filter(|c| !exclude.contains(&c.id)) {
                v.push(c.start);
                v.push(c.end());
            }
        }
        v
    }

    /// Pas de la grille de temps (échantillons) : la subdivision de la règle (quart de
    /// graduation), de plus en plus fine quand on zoome.
    pub fn grid_step(&self) -> u64 {
        ((self.ruler_step() / 4.0) * self.sr() as f64)
            .round()
            .max(1.0) as u64
    }

    /// Décalage à appliquer pour aimanter l'une des positions `edges` (déjà décalées).
    /// Priorité aux objets proches (repères, bords de prises, tête, sélection) ; sinon
    /// la position se cale sur le cran de grille le plus proche.
    fn snap_delta(&self, edges: &[i64], points: &[u64]) -> i64 {
        let tol = (SNAP_PX as f64 * self.view.spp) as i64;
        let mut best: Option<i64> = None;
        for &e in edges {
            for &p in points {
                let d = p as i64 - e;
                if d.abs() <= tol && best.is_none_or(|b| d.abs() < b.abs()) {
                    best = Some(d);
                }
            }
        }
        if best.is_none() {
            let g = self.grid_step() as i64;
            for &e in edges {
                let d = (e as f64 / g as f64).round() as i64 * g - e;
                if best.is_none_or(|b| d.abs() < b.abs()) {
                    best = Some(d);
                }
            }
        }
        best.unwrap_or(0)
    }

    /// Position aimantée (magnétisme actif et Maj relâchée). `moving_playhead` : la tête
    /// elle-même ne sert pas d'aimant (elle resterait collée à sa position).
    fn snap_pos(&self, s: u64, no_snap: bool, moving_playhead: bool) -> u64 {
        if !self.snap || no_snap {
            return s;
        }
        let mut pts = self.snap_points(&HashSet::new());
        if moving_playhead {
            pts.retain(|&p| p != self.playhead);
        }
        (s as i64 + self.snap_delta(&[s as i64], &pts)).max(0) as u64
    }

    /// Déplace la tête pendant qu'on la tire. En lecture, le son suit (au plus 12 fois
    /// par seconde : on ne relance pas la lecture disque à chaque image).
    fn scrub_to(&mut self, s: u64) {
        if self.is_stopped() {
            self.playhead = s;
        } else if self.now - self.scrub_at > 0.08 {
            self.scrub_at = self.now;
            self.seek(s);
        }
    }

    fn begin_clip_drag(
        &mut self,
        track_id: u64,
        clip: &Clip,
        mode: DragMode,
        anchor: u64,
        additive: bool,
    ) {
        if !self.sel_clips.contains(&clip.id) {
            if !additive {
                self.sel_clips.clear();
            }
            self.sel_clips.insert(clip.id);
        }
        let ids: HashSet<u64> = if mode == DragMode::Move {
            self.sel_clips.clone()
        } else {
            [clip.id].into()
        };
        let origs: Vec<(u64, Clip)> = self
            .project
            .tracks
            .iter()
            .flat_map(|t| {
                t.clips
                    .iter()
                    .filter(|c| ids.contains(&c.id))
                    .map(move |c| (t.id, c.clone()))
            })
            .collect();
        self.checkpoint();
        let _ = track_id;
        self.clip_drag = Some(ClipDrag {
            mode,
            anchor,
            main: clip.id,
            origs,
            target: None,
            changed: false,
        });
    }

    /// Met à jour le rectangle : sont sélectionnées les prises qu'il touche (en temps sur
    /// leur durée, en hauteur sur leur piste), en plus de la sélection d'origine.
    fn update_marquee(&mut self, cur: (u64, f32)) {
        let Some(m) = self.marquee.as_mut() else {
            return;
        };
        m.cur = cur;
        let (s0, s1) = (m.sample.min(cur.0), m.sample.max(cur.0));
        let (y0, y1) = (m.y.min(cur.1), m.y.max(cur.1));
        let mut sel = m.base.clone();
        for (i, t) in self.project.tracks.iter().enumerate() {
            let Some(&(a, b)) = self.view.row_spans.get(i) else {
                continue;
            };
            if b <= y0 || a >= y1 {
                continue;
            }
            sel.extend(
                t.clips
                    .iter()
                    .filter(|c| c.start < s1 && c.end() > s0)
                    .map(|c| c.id),
            );
        }
        self.sel_clips = sel;
    }

    fn update_clip_drag(&mut self, sample: u64, target_row: Option<usize>, no_snap: bool) {
        let Some(drag) = self.clip_drag.take() else {
            return;
        };
        if self.editing_locked() {
            // Prise lancée pendant le geste : on remet les prises où elles étaient.
            self.revert_clip_drag(&drag);
            return;
        }
        let mut d = sample as i64 - drag.anchor as i64;
        let exclude: HashSet<u64> = drag.origs.iter().map(|(_, c)| c.id).collect();
        let points = if self.snap && !no_snap {
            self.snap_points(&exclude)
        } else {
            Vec::new()
        };
        let main = drag
            .origs
            .iter()
            .find(|(_, c)| c.id == drag.main)
            .map(|(_, c)| c.clone());
        let mut target = drag.target;
        if let Some(m) = main {
            match drag.mode {
                DragMode::Move => {
                    let min_start =
                        drag.origs.iter().map(|(_, c)| c.start).min().unwrap_or(0) as i64;
                    d = d.max(-min_start);
                    if !points.is_empty() {
                        let s = m.start as i64 + d;
                        d += self.snap_delta(&[s, s + m.len as i64], &points);
                        d = d.max(-min_start);
                    }
                    for (_, o) in &drag.origs {
                        if let Some((ti, _)) = self.project.find_clip(o.id)
                            && let Some(c) = self.project.tracks[ti]
                                .clips
                                .iter_mut()
                                .find(|c| c.id == o.id)
                        {
                            c.start = (o.start as i64 + d) as u64;
                        }
                    }
                    if drag.origs.len() == 1 {
                        target = target_row.filter(|&r| r < self.project.tracks.len());
                    }
                }
                DragMode::TrimLeft => {
                    let (lo, hi) = trim_left_range(&m);
                    let mut ns = (m.start as i64 + d).clamp(lo, hi);
                    if !points.is_empty() {
                        ns = (ns + self.snap_delta(&[ns], &points)).clamp(lo, hi);
                    }
                    if let Some((ti, _)) = self.project.find_clip(m.id)
                        && let Some(c) = self.project.tracks[ti]
                            .clips
                            .iter_mut()
                            .find(|c| c.id == m.id)
                    {
                        c.start = ns as u64;
                        c.offset = (m.offset as i64 + (ns - m.start as i64)) as u64;
                        c.len = m.end() - ns as u64;
                        // Fondus repris depuis l'état d'origine : raccourcir puis rallonger
                        // ne les réduit pas définitivement.
                        c.fade_in = m.fade_in;
                        c.fade_out = m.fade_out;
                        c.clamp_fades();
                    }
                }
                DragMode::FadeIn | DragMode::FadeOut => {
                    let max_in = m.len.saturating_sub(m.fade_out) as i64;
                    let max_out = m.len.saturating_sub(m.fade_in) as i64;
                    if let Some((ti, _)) = self.project.find_clip(m.id)
                        && let Some(c) = self.project.tracks[ti]
                            .clips
                            .iter_mut()
                            .find(|c| c.id == m.id)
                    {
                        if drag.mode == DragMode::FadeIn {
                            c.fade_in = (m.fade_in as i64 + d).clamp(0, max_in) as u64;
                        } else {
                            c.fade_out = (m.fade_out as i64 - d).clamp(0, max_out) as u64;
                        }
                    }
                }
                DragMode::TrimRight => {
                    let (lo, hi) = trim_right_range(&m);
                    let mut ne = (m.end() as i64 + d).clamp(lo, hi);
                    if !points.is_empty() {
                        ne = (ne + self.snap_delta(&[ne], &points)).clamp(lo, hi);
                    }
                    if let Some((ti, _)) = self.project.find_clip(m.id)
                        && let Some(c) = self.project.tracks[ti]
                            .clips
                            .iter_mut()
                            .find(|c| c.id == m.id)
                    {
                        c.len = (ne - m.start as i64) as u64;
                        c.fade_in = m.fade_in;
                        c.fade_out = m.fade_out;
                        c.clamp_fades();
                    }
                }
            }
        }
        self.clip_drag = Some(ClipDrag {
            changed: drag.changed || d != 0 || target.is_some(),
            target,
            ..drag
        });
    }

    /// Annule un geste en cours (prises remises dans leur état d'origine).
    fn revert_clip_drag(&mut self, drag: &ClipDrag) {
        // Étape d'annulation empilée au début du geste : sans objet (une prise, elle, a
        // déjà vidé l'historique).
        if !self.is_recording() {
            self.undo.pop();
        }
        for (track_id, o) in &drag.origs {
            if let Some(t) = self.project.track_mut(*track_id)
                && let Some(c) = t.clips.iter_mut().find(|c| c.id == o.id)
            {
                *c = o.clone();
            }
        }
    }

    fn end_clip_drag(&mut self) {
        let Some(drag) = self.clip_drag.take() else {
            return;
        };
        if self.editing_locked() {
            self.revert_clip_drag(&drag);
            return;
        }
        let moved_track = match (drag.mode, drag.target, drag.origs.first()) {
            (DragMode::Move, Some(row), Some((from, _))) => match self.project.tracks.get(row) {
                Some(t) => (t.id != *from).then_some((*from, t.id)),
                None => None,
            },
            _ => None,
        };
        let unchanged = drag.origs.iter().all(|(_, o)| {
            self.project.find_clip(o.id).is_some_and(|(_, c)| {
                c.start == o.start
                    && c.len == o.len
                    && c.offset == o.offset
                    && c.fade_in == o.fade_in
                    && c.fade_out == o.fade_out
            })
        }) && moved_track.is_none();
        if unchanged {
            self.undo.pop();
            return;
        }
        if drag.mode == DragMode::Move {
            // Les prises déplacées passent au premier plan de leur piste.
            for (_, o) in &drag.origs {
                if let Some((ti, _)) = self.project.find_clip(o.id) {
                    let tr = &mut self.project.tracks[ti];
                    if let Some(i) = tr.clips.iter().position(|c| c.id == o.id) {
                        let c = tr.clips.remove(i);
                        let dest = match moved_track {
                            Some((_, to)) => self
                                .project
                                .tracks
                                .iter()
                                .position(|t| t.id == to)
                                .unwrap_or(ti),
                            None => ti,
                        };
                        self.project.tracks[dest].clips.push(c);
                    }
                }
            }
        }
        self.edited();
    }

    // ---------------------------------------------------------------- pistes

    fn tracks_area(&mut self, ui: &mut egui::Ui) {
        let full = ui.available_rect_before_wrap();
        let tl_left = full.left() + HEADER_W;
        let tl_width = full.right() - tl_left;
        self.view.width = tl_width;
        self.follow_playhead();

        let ruler_rect = Rect::from_min_max(
            pos2(tl_left, full.top()),
            pos2(full.right(), full.top() + RULER_H),
        );
        let corner = Rect::from_min_max(full.min, pos2(tl_left, full.top() + RULER_H));
        let painter = ui.painter().clone();
        painter.rect_filled(corner, 0.0, t::SURFACE_2);
        w::hline(
            &painter,
            corner.left(),
            corner.right(),
            corner.bottom() - 1.0,
            t::LINE,
        );
        let armed = self.project.tracks.iter().filter(|t| t.armed).count();
        painter.text(
            corner.left_center() + vec2(12.0, 0.0),
            Align2::LEFT_CENTER,
            format!(
                "{} PISTES · {} ARMÉE{}",
                self.project.tracks.len(),
                armed,
                if armed > 1 { "S" } else { "" }
            ),
            t::caps(9.5),
            t::TEXT_3,
        );
        // « + » au-dessus des pistes : ajouter des pistes (nombre, type, entrées).
        let add_r = Rect::from_center_size(
            pos2(corner.right() - 18.0, corner.center().y),
            vec2(22.0, 18.0),
        );
        let can_add = self.is_stopped();
        let add = ui.interact(
            add_r,
            ui.make_persistent_id("add_tracks"),
            if can_add {
                Sense::click()
            } else {
                Sense::hover()
            },
        );
        let hov = add.hovered() && can_add;
        if hov {
            painter.rect_filled(add_r, t::R, t::SURFACE_4);
        }
        w::paint_icon(
            &painter,
            Icon::Plus,
            add_r.center(),
            5.0,
            if !can_add {
                t::TEXT_3
            } else if hov {
                t::TEXT
            } else {
                t::TEXT_2
            },
        );
        if add
            .on_hover_text("Ajouter des pistes : une ou plusieurs, mono ou stéréo, sur les entrées de votre choix")
            .clicked()
        {
            self.open_new_tracks();
        }
        self.ruler(ui, ruler_rect);
        let sb_rect = Rect::from_min_max(pos2(tl_left, full.bottom() - SCROLLBAR_H), full.max);
        let tl_all = Rect::from_min_max(
            pos2(tl_left, full.top()),
            pos2(full.right(), full.bottom() - SCROLLBAR_H),
        );
        self.timeline_input(ui, tl_all);

        let body = Rect::from_min_max(
            pos2(full.left(), full.top() + RULER_H),
            pos2(full.right(), full.bottom() - SCROLLBAR_H),
        );
        let mut child = ui.new_child(UiBuilder::new().max_rect(body));
        let track_h = self.view.track_h;
        let mut to_delete = None;
        let mut clip_to_delete = None;
        let mut audible_changed = false;
        let stopped = self.is_stopped();
        // Pendant une prise, une piste non armée peut être armée : elle rejoint la prise.
        let recording = self.is_recording();
        let mut late_arm = None;
        let editable = !self.editing_locked();
        let in_ch = self.engine.status.in_channels;
        let in_base = self.in_base();
        let transport = self.transport;
        let blink = (self.now * 1.6).fract() < 0.55;
        let (pointer, press_origin, shift, command) = ui.input(|i| {
            (
                i.pointer.hover_pos(),
                i.pointer.press_origin(),
                i.modifiers.shift,
                i.modifiers.command,
            )
        });
        // Fichiers audio glissés depuis l'explorateur : position du curseur demandée au
        // système (egui ne reçoit aucun mouvement de souris pendant le survol).
        let files_hover = ui.input(|i| !i.raw.hovered_files.is_empty());
        let dropped: Vec<std::path::PathBuf> = ui.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .filter(|p| !p.as_os_str().is_empty())
                .collect()
        });
        let drop_ptr = if files_hover || !dropped.is_empty() {
            if files_hover {
                ui.ctx().request_repaint();
            }
            w::os_pointer_pos(ui.ctx()).or(pointer)
        } else {
            None
        };
        // Lâché sur la timeline : à cet endroit ; ailleurs : à la tête de lecture.
        let drop_at = match drop_ptr {
            Some(p) if p.x >= tl_left && full.contains(p) => self.sample_at(tl_left, p.x),
            _ => self.playhead,
        };
        let mut drop_target: Option<u64> = None;

        // Grille verticale (repères de la règle).
        let step = self.ruler_step();
        let sr = self.sr() as f64;
        // Lignes de la règle, et crans de la grille (plus discrets) quand le magnétisme
        // est actif : on voit où la tête, les coupes et les prises vont se caler.
        let grid_x: Vec<(f32, bool)> = {
            let t0 = self.view.scroll / sr;
            let t1 = t0 + tl_width as f64 * self.view.spp / sr;
            let mut v = Vec::new();
            let mut tt = (t0 / step).floor() * step;
            while tt <= t1 {
                v.push((self.x_of(tl_left, (tt * sr) as u64), true));
                if self.snap {
                    for k in 1..4 {
                        let tk = tt + step * k as f64 / 4.0;
                        v.push((self.x_of(tl_left, (tk * sr) as u64), false));
                    }
                }
                tt += step;
            }
            v
        };

        let mut sa = egui::ScrollArea::vertical()
            .id_salt("tracks_scroll")
            .auto_shrink([false, false]);
        let requested = self.view.want_scroll_y.is_some();
        let prev_y = self.view.scroll_y;
        if let Some(y) = self.view.want_scroll_y.take() {
            sa = sa.vertical_scroll_offset(y);
        }
        let mut lanes_top = body.top();
        let audible = self.project.audible();
        let out = sa.show(&mut child, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            lanes_top = ui.cursor().top();
            let ntracks = self.project.tracks.len();
            let mut spans = Vec::with_capacity(ntracks);
            for idx in 0..ntracks {
                let tid = self.project.tracks[idx].id;
                let expanded = self.expanded_tracks.contains(&tid);
                let is_audio = self.project.tracks[idx].is_audio();
                let row_h = track_h
                    + match (expanded, is_audio) {
                        (false, _) => 0.0,
                        (true, false) => INSPECTOR_H,
                        (true, true) => AUDIO_INSPECTOR_H,
                    };
                let (row, _) =
                    ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::hover());
                spans.push((row.top() - lanes_top, row.bottom() - lanes_top));
                let dragging_here = self
                    .clip_drag
                    .as_ref()
                    .is_some_and(|d| d.origs.iter().any(|(t, _)| *t == tid));
                if !ui.is_rect_visible(row) && !dragging_here {
                    continue;
                }
                let color = track_color(&self.project.tracks[idx], idx);
                let header = Rect::from_min_size(row.min, vec2(HEADER_W, track_h));
                let lane = Rect::from_min_max(pos2(row.left() + HEADER_W, row.top()), row.max);
                let selected = self.selected_tracks.contains(&tid);
                let rec_live = self.live.iter().any(|l| l.track_id == tid);
                let p = ui.painter().clone();
                p.rect_filled(lane, 0.0, if idx % 2 == 0 { t::LANE_A } else { t::LANE_B });
                let lp = ui.painter_at(lane);
                for &(gx, major) in &grid_x {
                    let col = if major { t::GRID } else { t::GRID.gamma_multiply(0.45) };
                    w::vline(&lp, gx, lane.top(), lane.bottom(), col);
                }
                if is_audio && drop_ptr.is_some_and(|dp| lane.contains(dp)) {
                    drop_target = Some(tid);
                    p.rect_filled(lane, 0.0, t::ACCENT.gamma_multiply(0.12));
                }
                p.rect_filled(
                    header,
                    0.0,
                    if selected { t::SURFACE_3 } else { t::SURFACE },
                );
                // Panneau déplié : fond un peu différent, sous l'en-tête.
                let panel = Rect::from_min_max(
                    pos2(row.left(), header.bottom()),
                    pos2(row.left() + HEADER_W, row.bottom()),
                );
                if expanded {
                    p.rect_filled(panel, 0.0, t::SURFACE_2);
                    w::hline(&p, panel.left() + 3.0, panel.right(), panel.top(), t::LINE);
                }
                p.rect_filled(
                    Rect::from_min_size(header.min, vec2(3.0, row_h)),
                    0.0,
                    if rec_live { t::REC } else { color },
                );
                w::hline(&p, row.left(), row.right(), row.bottom() - 1.0, t::LINE);
                w::vline(&p, header.right() - 1.0, row.top(), row.bottom(), t::LINE);
                if let Some(d) = &self.clip_drag
                    && d.target == Some(idx)
                {
                    p.rect_stroke(
                        lane.shrink(1.0),
                        0.0,
                        Stroke::new(1.0, t::ACCENT.gamma_multiply(0.6)),
                        StrokeKind::Inside,
                    );
                }

                let hresp =
                    ui.interact(header, ui.make_persistent_id(("hdr", tid)), Sense::click());
                if hresp.clicked() {
                    if command || shift {
                        if !self.selected_tracks.remove(&tid) {
                            self.selected_tracks.insert(tid);
                        }
                    } else {
                        self.selected_tracks.clear();
                        self.selected_tracks.insert(tid);
                    }
                }
                // Clic droit n'importe où sur la carte (le nom n'est plus un champ de saisie
                // permanent : il ne capture plus le clic droit).
                let mut toggle_panel = false;
                hresp.context_menu(|ui| {
                    if ui
                        .button(match (expanded, is_audio) {
                            (true, _) => "Masquer les réglages de la piste",
                            (false, false) => "Entrée et réglages de la piste…",
                            (false, true) => "Couleur de la piste…",
                        })
                        .clicked()
                    {
                        toggle_panel = true;
                        ui.close();
                    }
                    if ui
                        .add_enabled(
                            stopped,
                            egui::Button::new("Renommer").shortcut_text("double-clic"),
                        )
                        .clicked()
                    {
                        self.renaming_track = Some(tid);
                        self.renaming_focus = true;
                        ui.close();
                    }
                    if ui
                        .button("Sélectionner toutes les prises de la piste")
                        .clicked()
                    {
                        self.sel_clips = self.project.tracks[idx]
                            .clips
                            .iter()
                            .map(|c| c.id)
                            .collect();
                        ui.close();
                    }
                    if ui
                        .add_enabled(stopped, egui::Button::new("Supprimer la piste"))
                        .clicked()
                    {
                        to_delete = Some(idx);
                        ui.close();
                    }
                });

                // ---- en-tête
                let mut hdr = ui.new_child(
                    UiBuilder::new()
                        .max_rect(header.shrink2(vec2(10.0, 7.0)).translate(vec2(3.0, 0.0))),
                );
                hdr.spacing_mut().item_spacing = vec2(4.0, 6.0);
                let mut name_rect = Rect::NOTHING;
                let track = &mut self.project.tracks[idx];
                hdr.horizontal(|ui| {
                    // Flèche : déplier le panneau de réglages (entrée, type, monitoring…).
                    let (ar, aresp) = ui.allocate_exact_size(vec2(12.0, 18.0), Sense::click());
                    let acol = if aresp.hovered() { t::TEXT } else { t::TEXT_2 };
                    let c = ar.center();
                    let tri = if expanded {
                        vec![pos2(c.x - 4.0, c.y - 2.0), pos2(c.x + 4.0, c.y - 2.0), pos2(c.x, c.y + 3.0)]
                    } else {
                        vec![pos2(c.x - 2.0, c.y - 4.0), pos2(c.x + 3.0, c.y), pos2(c.x - 2.0, c.y + 4.0)]
                    };
                    ui.painter().add(egui::Shape::convex_polygon(tri, acol, Stroke::NONE));
                    if aresp
                        .on_hover_text(if is_audio {
                            "Réglages de la piste : couleur"
                        } else {
                            "Réglages de la piste : entrée, mono / stéréo, monitoring"
                        })
                        .clicked()
                    {
                        toggle_panel = true;
                    }
                    if is_audio {
                        // Piste audio : fichiers importés, lecture seule (pas d'enregistrement).
                        let (r, resp) = ui.allocate_exact_size(vec2(20.0, 18.0), Sense::hover());
                        ui.painter().rect_filled(r, t::R, t::SURFACE_3);
                        w::paint_icon(ui.painter(), Icon::Speaker, r.center(), 4.5, t::TEXT_2);
                        resp.on_hover_text(
                            "Piste audio : fichiers importés, en lecture seule (pas d'entrée, pas d'enregistrement)",
                        );
                    } else if w::arm_button(
                        ui,
                        track.armed,
                        (stopped || (recording && !rec_live)) && track.input.is_some(),
                        !rec_live || blink,
                    )
                    .clicked()
                    {
                        // Pendant une prise : la piste (non armée, ou armée mais écartée au
                        // départ faute d'entrée) rejoint la prise.
                        if recording {
                            track.armed = true;
                            late_arm = Some(tid);
                        } else {
                            track.armed = !track.armed;
                        }
                        self.project.dirty = true;
                    }
                    ui.add_space(2.0);
                    // Nom : simple texte (les clics vont à la carte) ; champ de saisie
                    // seulement pendant un renommage (double-clic ou menu « Renommer »).
                    if self.renaming_track == Some(tid) && stopped {
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut track.name)
                                .desired_width(136.0)
                                .font(t::medium(12.0))
                                .margin(egui::Margin::symmetric(2, 1)),
                        );
                        if self.renaming_focus {
                            // Nom entièrement sélectionné : on retape directement.
                            self.renaming_focus = false;
                            r.request_focus();
                            if let Some(mut st) = egui::TextEdit::load_state(ui.ctx(), r.id) {
                                let len = track.name.chars().count();
                                st.cursor.set_char_range(Some(egui::text::CCursorRange::two(
                                    egui::text::CCursor::new(0),
                                    egui::text::CCursor::new(len),
                                )));
                                st.store(ui.ctx(), r.id);
                            }
                        } else if r.lost_focus() {
                            // Entrée, Échap ou clic ailleurs : fin du renommage.
                            self.renaming_track = None;
                        }
                        if r.changed() {
                            self.project.dirty = true;
                        }
                        name_rect = r.rect;
                    } else {
                        let (nr, _) = ui.allocate_exact_size(vec2(140.0, 18.0), Sense::hover());
                        let font = t::medium(12.0);
                        let shown = w::fit_text(ui, &track.name, &font, nr.width() - 4.0);
                        ui.painter().text(
                            pos2(nr.left() + 2.0, nr.center().y),
                            Align2::LEFT_CENTER,
                            shown,
                            font,
                            t::TEXT,
                        );
                        name_rect = nr;
                    }
                    // Monitoring direct : écouter en temps réel ce qui entre dans la piste.
                    if is_audio {
                        ui.allocate_exact_size(vec2(20.0, 18.0), Sense::hover());
                    } else if w::icon_tag_button(
                        ui,
                        Icon::Headphones,
                        track.monitor,
                        t::PLAY,
                        track.input.is_some(),
                        "Monitoring : écouter l'entrée en direct (sortie « Monitoring » des réglages de la carte son)",
                    )
                    .clicked()
                    {
                        track.monitor = !track.monitor;
                        self.project.dirty = true;
                    }
                    if w::tag_button(ui, "M", track.mute, t::ORANGE, true, "Muet (lecture)")
                        .clicked()
                    {
                        track.mute = !track.mute;
                        audible_changed = true;
                    }
                    if w::tag_button(ui, "S", track.solo, t::YELLOW, true, "Solo (lecture)")
                        .clicked()
                    {
                        track.solo = !track.solo;
                        audible_changed = true;
                    }
                });
                // Double-clic sur le nom : renommer.
                if stopped
                    && hresp.double_clicked()
                    && hresp
                        .interact_pointer_pos()
                        .is_some_and(|p| name_rect.contains(p))
                {
                    self.renaming_track = Some(tid);
                    self.renaming_focus = true;
                }
                let input = track.input;
                let stereo = track.stereo;
                let label = track.input_label();
                let chans: Vec<u16> = track.inputs().collect();
                let ms: Vec<MeterState> = if shows_playback(track, transport) {
                    // Niveau de lecture (après le fader), une barre par canal.
                    let m = self.track_meters.get(&tid).copied().unwrap_or_default();
                    m[..track.channels()].to_vec()
                } else {
                    chans
                        .iter()
                        .map(|&i| match i.checked_sub(in_base) {
                            Some(c) if (c as usize) < audio::MAX_INPUTS && c < in_ch => {
                                self.in_meters[c as usize]
                            }
                            _ => MeterState::default(),
                        })
                        .collect()
                };
                let mut new_input: Option<Option<u16>> = None;
                let mut new_stereo: Option<bool> = None;
                let (mut arm_toggle, mut monitor_toggle) = (false, false);
                let mut new_color: Option<u8> = None;
                hdr.horizontal(|ui| {
                    if is_audio {
                        let (r, resp) = ui.allocate_exact_size(vec2(58.0, 18.0), Sense::hover());
                        ui.painter().rect_filled(r, t::R, t::INSET);
                        ui.painter().text(
                            pos2(r.left() + 6.0, r.center().y),
                            Align2::LEFT_CENTER,
                            if stereo { "STÉRÉO" } else { "MONO" },
                            t::medium(10.0),
                            t::TEXT_3,
                        );
                        resp.on_hover_text("Mono ou stéréo selon les fichiers importés");
                    } else {
                    // Étiquette d'entrée présentée en liste déroulante (« IN 1 ▾ »).
                    let (r, resp) = ui.allocate_exact_size(
                        vec2(58.0, 18.0),
                        if stopped {
                            Sense::click()
                        } else {
                            Sense::hover()
                        },
                    );
                    let hov = resp.hovered() && stopped;
                    ui.painter()
                        .rect_filled(r, t::R, if hov { t::SURFACE_4 } else { t::INSET });
                    let fg = if input.is_some() { t::TEXT } else { t::TEXT_3 };
                    ui.painter().text(
                        pos2(r.left() + 6.0, r.center().y),
                        Align2::LEFT_CENTER,
                        label,
                        t::medium(10.0),
                        fg,
                    );
                    let cx = r.right() - 8.0;
                    let cy = r.center().y;
                    ui.painter().add(egui::Shape::convex_polygon(
                        vec![pos2(cx - 3.0, cy - 1.5), pos2(cx + 3.0, cy - 1.5), pos2(cx, cy + 2.0)],
                        if stopped { t::TEXT_2 } else { t::TEXT_3 },
                        Stroke::NONE,
                    ));
                    let resp = resp.on_hover_text("Mono / stéréo et entrée de la carte");
                    egui::Popup::menu(&resp)
                        .width(picker_width(stereo))
                        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                        .show(|ui| {
                            let mut s = stereo;
                            if w::segmented(
                                ui,
                                "trackmode",
                                &mut s,
                                &[(false, "Mono"), (true, "Stéréo")],
                            ) {
                                new_stereo = Some(s);
                            }
                            ui.add_space(6.0);
                            // En changeant de mode, l'entrée est d'abord recalée (paire paire).
                            let cur = input.map(|i| if s { i & !1 } else { i });
                            new_input =
                                input_picker(ui, cur, s, in_ch, in_base, &self.project.tracks, Some(tid));
                            if new_input.is_some() {
                                ui.close();
                            }
                        });
                    }
                    let (mr, _) = ui.allocate_exact_size(vec2(152.0, 18.0), Sense::hover());
                    if ms.len() == 2 {
                        // Stéréo : deux barres bien distinctes, entrée gauche en haut, droite en
                        // bas (chacune son propre signal).
                        for (k, m) in ms.iter().enumerate() {
                            let y = mr.center().y - 3.5 + k as f32 * 7.0;
                            w::meter_h(
                                ui.painter(),
                                Rect::from_center_size(pos2(mr.center().x, y), vec2(mr.width(), 5.0)),
                                m,
                            );
                        }
                    } else {
                        w::meter_h(
                            ui.painter(),
                            Rect::from_center_size(mr.center(), vec2(mr.width(), 6.0)),
                            &ms.first().copied().unwrap_or_default(),
                        );
                    }
                    let merged = ms
                        .iter()
                        .copied()
                        .reduce(MeterState::merge)
                        .unwrap_or_default();
                    let (pr, presp) = ui.allocate_exact_size(vec2(36.0, 16.0), Sense::click());
                    w::peak_readout(ui.painter(), pr, &merged);
                    if presp
                        .on_hover_text("Crête — clic : remise à zéro")
                        .clicked()
                    {
                        if let Some(m) = self.track_meters.get_mut(&tid) {
                            m[0].reset();
                            m[1].reset();
                        }
                        for &i in &chans {
                            if let Some(mm) = i
                                .checked_sub(in_base)
                                .and_then(|c| self.in_meters.get_mut(c as usize))
                            {
                                mm.reset();
                            }
                        }
                    }
                });
                if expanded {
                    let mut pui = ui.new_child(
                        UiBuilder::new().max_rect(panel.shrink2(vec2(13.0, 8.0)).translate(vec2(2.0, 0.0))),
                    );
                    let track = &self.project.tracks[idx];
                    let (stereo, input, armed, monitor) =
                        (track.stereo, track.input, track.armed, track.monitor);
                    let cur_color = track_color(track, idx);
                    egui::Grid::new(("track_panel", tid))
                        .num_columns(2)
                        .spacing([10.0, 6.0])
                        .show(&mut pui, |ui| {
                            if !is_audio {
                            ui.label(egui::RichText::new("Type").font(t::regular(11.5)).color(t::TEXT_2));
                            let mut s = stereo;
                            ui.add_enabled_ui(stopped, |ui| {
                                if w::segmented(ui, &format!("tp_type{tid}"), &mut s, &[(false, "Mono"), (true, "Stéréo")]) {
                                    new_stereo = Some(s);
                                }
                            });
                            ui.end_row();
                            ui.label(egui::RichText::new("Entrée").font(t::regular(11.5)).color(t::TEXT_2));
                            let mut cur = input.map(|i| if s { i & !1 } else { i });
                            ui.add_enabled_ui(stopped, |ui| {
                                if input_combo(ui, &format!("tp_in{tid}"), 186.0, &mut cur, s, in_ch, in_base, &self.project.tracks, Some(tid)) {
                                    new_input = Some(cur);
                                }
                            });
                            ui.end_row();
                            }
                            ui.label(egui::RichText::new("Couleur").font(t::regular(11.5)).color(t::TEXT_2));
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 2.0;
                                for (i, c) in t::PALETTE.iter().enumerate() {
                                    let (r, resp) = ui.allocate_exact_size(vec2(15.0, 18.0), Sense::click());
                                    ui.painter().circle_filled(r.center(), 5.5, *c);
                                    if *c == cur_color {
                                        ui.painter().circle_stroke(r.center(), 7.0, Stroke::new(1.2, t::TEXT));
                                    } else if resp.hovered() {
                                        ui.painter().circle_stroke(r.center(), 7.0, Stroke::new(1.0, t::TEXT_3));
                                    }
                                    if resp.clicked() {
                                        new_color = Some(i as u8);
                                    }
                                }
                            });
                            ui.end_row();
                            if is_audio {
                                return;
                            }
                            ui.label("");
                            ui.horizontal(|ui| {
                                if w::tool_button(ui, Some(Icon::Record), "Armée", armed, (stopped || (recording && !rec_live)) && input.is_some())
                                    .on_hover_text("Enregistrer cette piste à la prochaine prise")
                                    .clicked()
                                {
                                    arm_toggle = true;
                                }
                                if w::tool_button(ui, Some(Icon::Headphones), "Monitoring", monitor, input.is_some())
                                    .on_hover_text("Écouter l'entrée en direct (sortie « Monitoring »)")
                                    .clicked()
                                {
                                    monitor_toggle = true;
                                }
                            });
                            ui.end_row();
                        });
                }
                if let Some(c) = new_color {
                    self.project.tracks[idx].color = Some(c);
                    self.project.dirty = true;
                }
                if arm_toggle || monitor_toggle {
                    let track = &mut self.project.tracks[idx];
                    if arm_toggle {
                        if recording {
                            track.armed = true;
                            late_arm = Some(tid);
                        } else {
                            track.armed = !track.armed;
                        }
                    }
                    if monitor_toggle {
                        track.monitor = !track.monitor;
                    }
                    self.project.dirty = true;
                }
                if toggle_panel && !self.expanded_tracks.remove(&tid) {
                    self.expanded_tracks.insert(tid);
                }
                if new_stereo.is_some() || new_input.is_some() {
                    let track = &mut self.project.tracks[idx];
                    if let Some(s) = new_stereo {
                        track.stereo = s;
                        if s {
                            track.input = track.input.map(|i| i & !1);
                        }
                    }
                    if let Some(ni) = new_input {
                        track.input = ni;
                    }
                    if track.input.is_none() {
                        track.armed = false;
                    }
                    self.project.dirty = true;
                }

                // ---- prises
                let inner = lane.shrink2(vec2(0.0, 3.0));
                let muted = !audible.get(idx).copied().unwrap_or(true);
                let ccol = if muted {
                    t::mix(color, t::TEXT_3, 0.75)
                } else {
                    color
                };
                let track = &self.project.tracks[idx];
                let mut hovered_clip: Option<(Clip, DragMode)> = None;
                let mut pressed_clip: Option<(Clip, DragMode)> = None;
                for c in &track.clips {
                    let x0 = self.x_of(lane.left(), c.start);
                    let x1 = self.x_of(lane.left(), c.end());
                    if x1 < lane.left() || x0 > lane.right() {
                        continue;
                    }
                    let crect = Rect::from_min_max(
                        pos2(x0, inner.top()),
                        pos2(x1.max(x0 + 2.0), inner.bottom()),
                    );
                    let sel = self.sel_clips.contains(&c.id);
                    let hov = pointer.is_some_and(|pp| crect.contains(pp));
                    draw_clip(
                        &lp,
                        crect,
                        lane,
                        ccol,
                        self.peaks.get(&c.id),
                        c.offset,
                        self.view.spp,
                        Some(&c.file),
                        sel,
                        (c.fade_in, c.fade_out),
                        (sel || hov) && editable,
                    );
                    let hit = |pp: egui::Pos2| -> Option<(Clip, DragMode)> {
                        if !(crect.contains(pp) && lane.contains(pp)) {
                            return None;
                        }
                        // Bandeau du haut : poignées de fondu ; corps : bords = rognage.
                        let fx_in = crect.left() + (c.fade_in as f64 / self.view.spp) as f32;
                        let fx_out = crect.right() - (c.fade_out as f64 / self.view.spp) as f32;
                        let in_head = pp.y < crect.top() + CLIP_HEADER + 1.0;
                        let mode = if in_head
                            && (pp.x - fx_in).abs() < EDGE_GRAB
                            && crect.width() > 3.0 * EDGE_GRAB
                        {
                            DragMode::FadeIn
                        } else if in_head
                            && (pp.x - fx_out).abs() < EDGE_GRAB
                            && crect.width() > 3.0 * EDGE_GRAB
                        {
                            DragMode::FadeOut
                        } else if crect.width() > 3.0 * EDGE_GRAB && pp.x - crect.left() < EDGE_GRAB
                        {
                            DragMode::TrimLeft
                        } else if crect.width() > 3.0 * EDGE_GRAB
                            && crect.right() - pp.x < EDGE_GRAB
                        {
                            DragMode::TrimRight
                        } else {
                            DragMode::Move
                        };
                        Some((c.clone(), mode))
                    };
                    // Les prises dessinées ensuite sont au-dessus : la dernière touchée gagne.
                    if let Some(h) = pointer.and_then(hit) {
                        hovered_clip = Some(h);
                    }
                    if let Some(h) = press_origin.and_then(hit) {
                        pressed_clip = Some(h);
                    }
                }
                // Prise en cours, ou en finalisation (fichiers en train d'être fermés) : elle
                // reste affichée jusqu'à son intégration à la session.
                let live_span = match self.transport {
                    Transport::Recording { start } => Some((start, None)),
                    _ => self
                        .finishing
                        .as_ref()
                        .filter(|f| f.dir() == self.project.dir)
                        .map(|f| (f.start, Some(()))),
                };
                if let Some((start, finishing)) = live_span
                    && let Some(l) = self.live.iter().find(|l| l.track_id == track.id)
                {
                    // Piste armée pendant la prise : elle commence plus loin.
                    let start = start + l.offset;
                    let end = if finishing.is_some() {
                        start + (l.peaks.read().l0.len() * crate::render::PEAK_BLOCK) as u64
                    } else {
                        self.playhead
                    };
                    let x0 = self.x_of(lane.left(), start);
                    let x1 = self.x_of(lane.left(), end);
                    let crect = Rect::from_min_max(
                        pos2(x0, inner.top()),
                        pos2(x1.max(x0 + 2.0), inner.bottom()),
                    );
                    draw_clip(
                        &lp,
                        crect,
                        lane,
                        t::REC,
                        Some(&l.peaks),
                        0,
                        self.view.spp,
                        Some("Enregistrement…"),
                        false,
                        (0, 0),
                        false,
                    );
                }

                // ---- interactions sur la piste
                let resp = ui.interact(
                    lane,
                    ui.make_persistent_id(("lane", tid)),
                    Sense::click_and_drag(),
                );
                if self.clip_drag.is_none() && resp.hovered() && editable {
                    match hovered_clip.as_ref().map(|h| h.1) {
                        Some(DragMode::TrimLeft)
                        | Some(DragMode::TrimRight)
                        | Some(DragMode::FadeIn)
                        | Some(DragMode::FadeOut) => {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal)
                        }
                        Some(DragMode::Move) => ui.ctx().set_cursor_icon(egui::CursorIcon::Grab),
                        None => {}
                    }
                }
                if resp.drag_started() {
                    // Le geste est reconnu après quelques pixels : on part du point d'appui.
                    let pp = press_origin
                        .or(resp.interact_pointer_pos())
                        .unwrap_or_default();
                    let s = self.sample_at(lane.left(), pp.x);
                    match (&pressed_clip, editable) {
                        (Some((c, mode)), true) => {
                            let c = c.clone();
                            self.begin_clip_drag(tid, &c, *mode, s, command);
                        }
                        _ => {
                            // Zone vide : rectangle de sélection des prises (la sélection de
                            // temps se fait en glissant sur la règle).
                            let y = pp.y - lanes_top;
                            let base = if command || shift {
                                self.sel_clips.clone()
                            } else {
                                HashSet::new()
                            };
                            self.marquee = Some(Marquee {
                                sample: s,
                                y,
                                base,
                                cur: (s, y),
                            });
                        }
                    }
                }
                if resp.dragged() {
                    if self.clip_drag.is_some() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
                        if let Some(pp) = resp.interact_pointer_pos() {
                            let s = self.sample_at(lane.left(), pp.x);
                            // Piste sous le pointeur, d'après les vraies hauteurs (dépliées ou non).
                            let y = pp.y - lanes_top;
                            let target = self
                                .view
                                .row_spans
                                .iter()
                                .position(|&(a, b)| y >= a && y < b);
                            self.update_clip_drag(s, target, shift);
                        }
                    } else if self.marquee.is_some()
                        && let Some(pp) = resp.interact_pointer_pos()
                    {
                        let cur = (self.sample_at(lane.left(), pp.x), pp.y - lanes_top);
                        self.update_marquee(cur);
                    }
                }
                if resp.drag_stopped() {
                    if self.clip_drag.is_some() {
                        self.end_clip_drag();
                    } else {
                        self.marquee = None;
                    }
                }
                if resp.clicked() {
                    match &hovered_clip {
                        Some((c, _)) => {
                            if command {
                                if !self.sel_clips.remove(&c.id) {
                                    self.sel_clips.insert(c.id);
                                }
                            } else {
                                self.sel_clips.clear();
                                self.sel_clips.insert(c.id);
                            }
                        }
                        None => {
                            self.sel_clips.clear();
                            if !self.is_recording()
                                && let Some(pp) = resp.interact_pointer_pos()
                            {
                                let s = self.sample_at(lane.left(), pp.x);
                                let s = self.snap_pos(s, shift, true);
                                self.seek(s);
                            }
                        }
                    }
                }
                if resp.double_clicked()
                    && let Some((c, _)) = &hovered_clip
                {
                    self.selection = Some((c.start, c.end()));
                }
                if resp.secondary_clicked() {
                    self.ctx_clip = hovered_clip.as_ref().map(|(c, _)| (tid, c.id));
                    if let Some((c, _)) = &hovered_clip
                        && !self.sel_clips.contains(&c.id)
                    {
                        self.sel_clips.clear();
                        self.sel_clips.insert(c.id);
                    }
                }
                resp.context_menu(|ui| match self.ctx_clip {
                    Some((tt, c)) if tt == tid => {
                        if ui
                            .add_enabled(
                                editable,
                                egui::Button::new("Couper à la tête de lecture   S"),
                            )
                            .clicked()
                        {
                            self.split_at_playhead();
                            ui.close();
                        }
                        ui.separator();
                        for (label, ms) in [
                            ("Fondus courts (10 ms)", 10u64),
                            ("Fondus 100 ms", 100),
                            ("Fondus 1 s", 1000),
                        ] {
                            if ui.add_enabled(editable, egui::Button::new(label)).clicked() {
                                self.set_clip_fades(Some(ms));
                                ui.close();
                            }
                        }
                        if ui
                            .add_enabled(editable, egui::Button::new("Retirer les fondus"))
                            .clicked()
                        {
                            self.set_clip_fades(None);
                            ui.close();
                        }
                        ui.separator();
                        if ui.button("Sélectionner la plage de la prise").clicked() {
                            if let Some((_, cl)) = self.project.find_clip(c) {
                                self.selection = Some((cl.start, cl.end()));
                            }
                            ui.close();
                        }
                        if ui
                            .add_enabled(editable, egui::Button::new("Supprimer   Suppr"))
                            .clicked()
                        {
                            clip_to_delete = Some(());
                            ui.close();
                        }
                    }
                    _ => {
                        ui.label(egui::RichText::new("Aucune prise ici").color(t::TEXT_3));
                    }
                });
            }
            // Sous la dernière piste, la timeline et la colonne des pistes continuent jusqu'en
            // bas : on clique (aller à cet endroit) et on trace un rectangle de sélection depuis
            // n'importe où.
            let rest_top = ui.cursor().top();
            let rest_h = (ui.clip_rect().bottom() - rest_top).max(60.0);
            let (rest, _) =
                ui.allocate_exact_size(vec2(ui.available_width(), rest_h), Sense::hover());
            let rest_head = Rect::from_min_size(rest.min, vec2(HEADER_W, rest_h));
            let rest_lane = Rect::from_min_max(pos2(rest.left() + HEADER_W, rest.top()), rest.max);
            {
                let p = ui.painter();
                p.rect_filled(rest_head, 0.0, t::SURFACE);
                p.rect_filled(rest_lane, 0.0, t::LANE_A);
                let lp = ui.painter_at(rest_lane);
                for &(gx, major) in &grid_x {
                    let col = if major { t::GRID } else { t::GRID.gamma_multiply(0.45) };
                    w::vline(&lp, gx, rest_lane.top(), rest_lane.bottom(), col);
                }
                w::vline(p, rest_head.right() - 1.0, rest.top(), rest.bottom(), t::LINE);
                if ntracks == 0 {
                    p.text(
                        pos2(rest_lane.center().x, rest.top() + 50.0),
                        Align2::CENTER_CENTER,
                        "Aucune piste",
                        t::semibold(13.0),
                        t::TEXT_2,
                    );
                    p.text(
                        pos2(rest_lane.center().x, rest.top() + 72.0),
                        Align2::CENTER_CENTER,
                        "Bouton « + » juste au-dessus, à droite du nombre de pistes : une ou plusieurs d'un coup, sur les entrées de votre choix.",
                        t::regular(12.0),
                        t::TEXT_3,
                    );
                }
            }
            let rresp = ui.interact(
                rest_lane,
                ui.make_persistent_id("lanes_rest"),
                Sense::click_and_drag(),
            );
            let (command, shift) = ui.input(|i| (i.modifiers.command, i.modifiers.shift));
            if rresp.drag_started() {
                let pp = ui
                    .input(|i| i.pointer.press_origin())
                    .or(rresp.interact_pointer_pos())
                    .unwrap_or_default();
                let s = self.sample_at(rest_lane.left(), pp.x);
                let y = pp.y - lanes_top;
                let base = if command || shift {
                    self.sel_clips.clone()
                } else {
                    HashSet::new()
                };
                self.marquee = Some(Marquee {
                    sample: s,
                    y,
                    base,
                    cur: (s, y),
                });
            }
            if rresp.dragged()
                && self.marquee.is_some()
                && let Some(pp) = rresp.interact_pointer_pos()
            {
                let cur = (self.sample_at(rest_lane.left(), pp.x), pp.y - lanes_top);
                self.update_marquee(cur);
            }
            if rresp.drag_stopped() {
                self.marquee = None;
            }
            if rresp.clicked() {
                self.sel_clips.clear();
                if !self.is_recording()
                    && let Some(pp) = rresp.interact_pointer_pos()
                {
                    let s = self.sample_at(rest_lane.left(), pp.x);
                    let s = self.snap_pos(s, shift, true);
                    self.seek(s);
                }
            }
            self.view.row_spans = spans;
            if let Some(m) = &self.marquee {
                let x0 = self.x_of(tl_left, m.sample);
                let x1 = self.x_of(tl_left, m.cur.0);
                let r = Rect::from_two_pos(
                    pos2(x0.max(tl_left), lanes_top + m.y),
                    pos2(x1.max(tl_left), lanes_top + m.cur.1),
                );
                let p = ui.painter();
                p.rect_filled(r, 0.0, t::ACCENT.gamma_multiply(0.10));
                p.rect_stroke(r, 0.0, Stroke::new(1.0, t::ACCENT.gamma_multiply(0.8)), StrokeKind::Inside);
            }
        });
        self.view.scroll_y = out.state.clamped_offset().y;
        self.view.viewport_h = out.inner_rect.height();
        if !requested && (self.view.scroll_y - prev_y).abs() > 0.5 {
            self.user_navigated();
        }

        // Surimpressions sur toute la hauteur : sélection, repères, tête de lecture.
        let over = Rect::from_min_max(pos2(tl_left, body.top()), body.max);
        let op = ui.painter_at(over);
        if let Some((a, b)) = self.selection {
            let xa = self.x_of(tl_left, a).max(tl_left);
            let xb = self.x_of(tl_left, b).min(full.right());
            if xb > xa {
                op.rect_filled(
                    Rect::from_min_max(pos2(xa, over.top()), pos2(xb, over.bottom())),
                    0.0,
                    t::SELECTION,
                );
                w::vline(
                    &op,
                    xa,
                    over.top(),
                    over.bottom(),
                    t::ACCENT.gamma_multiply(0.5),
                );
                w::vline(
                    &op,
                    xb,
                    over.top(),
                    over.bottom(),
                    t::ACCENT.gamma_multiply(0.5),
                );
            }
        }
        for m in &self.project.markers {
            let x = self.x_of(tl_left, m.pos);
            if over.x_range().contains(x) {
                w::vline(
                    &op,
                    x,
                    over.top(),
                    over.bottom(),
                    t::MARKER.gamma_multiply(0.35),
                );
            }
        }
        let x = self.x_of(tl_left, self.playhead);
        if over.x_range().contains(x) {
            let col = if self.is_recording() {
                t::REC
            } else {
                t::PLAYHEAD
            };
            w::vline(&op, x, over.top(), over.bottom(), col);
        }
        self.scrollbar(ui, sb_rect);
        ui.painter().rect_filled(
            Rect::from_min_max(
                pos2(full.left(), sb_rect.top()),
                pos2(tl_left, full.bottom()),
            ),
            0.0,
            t::SURFACE,
        );

        if files_hover && let Some(dp) = drop_ptr {
            let fp = ui.ctx().layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::unique("drop_overlay"),
            ));
            fp.rect_stroke(
                full.shrink(1.0),
                0.0,
                Stroke::new(2.0, t::ACCENT),
                StrokeKind::Inside,
            );
            if dp.x >= tl_left && full.contains(dp) {
                let x = self.x_of(tl_left, drop_at);
                w::vline(&fp, x, full.top(), full.bottom(), t::ACCENT);
            }
            let msg = if drop_target.is_some() {
                "Déposer : ajouter le fichier à cette piste audio"
            } else {
                "Déposer : une nouvelle piste audio par fichier"
            };
            let g = fp.layout_no_wrap(msg.into(), t::semibold(12.0), t::TEXT);
            let r = Rect::from_center_size(
                pos2(full.center().x, full.top() + RULER_H + 30.0),
                g.size() + vec2(28.0, 14.0),
            );
            fp.rect_filled(r, t::R, t::SURFACE_3);
            fp.rect_stroke(r, t::R, Stroke::new(1.0, t::ACCENT), StrokeKind::Inside);
            fp.galley(r.center() - g.size() / 2.0, g, t::TEXT);
        }
        if !dropped.is_empty() {
            self.import_audio_files(dropped, drop_at, drop_target);
        }
        if audible_changed {
            self.project.dirty = true;
            self.update_audible();
        }
        if let Some(i) = to_delete {
            self.checkpoint();
            let id = self.project.tracks[i].id;
            self.selected_tracks.remove(&id);
            self.project.tracks.remove(i);
            self.edited();
        }
        if let Some(tid) = late_arm
            && !self.arm_during_take(tid)
            && let Some(t) = self.project.tracks.iter_mut().find(|t| t.id == tid)
        {
            t.armed = false;
        }
        if clip_to_delete.is_some() {
            self.delete_selection();
        }
    }

    // ---------------------------------------------------------------- bandeau de vumètres

    fn meter_bridge(&mut self, ui: &mut egui::Ui) {
        let full = ui.max_rect();
        let in_ch = self.engine.status.in_channels;
        let in_base = self.in_base();
        let transport = self.transport;
        let p = ui.painter().clone();
        w::hline(&p, full.left(), full.right(), full.top(), t::LINE);
        // Zones : crête, panoramique, puis vumètre et fader côte à côte, puis noms.
        let top = full.top() + 62.0;
        let bottom = full.bottom() - 34.0;
        p.text(
            pos2(full.left() + 12.0, full.top() + 12.0),
            Align2::LEFT_CENTER,
            "PISTES",
            t::caps(10.0),
            t::TEXT_3,
        );
        w::scale_v(
            &p,
            Rect::from_min_max(
                pos2(full.left() + 8.0, top),
                pos2(full.left() + 24.0, bottom),
            ),
            true,
        );
        let area = Rect::from_min_max(pos2(full.left() + 30.0, full.top() + 22.0), full.max);
        let mut child = ui.new_child(UiBuilder::new().max_rect(area));
        let mut set_gain: Option<(usize, f32)> = None;
        let mut set_pan: Option<(usize, f32)> = None;
        let mut set_master: Option<f32> = None;
        let mut reset_master = false;
        egui::ScrollArea::horizontal()
            .id_salt("bridge")
            .auto_shrink([false, false])
            .show(&mut child, |ui| {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 1.0;
                    let col_w = BRIDGE_COL_W;
                    // Aucune allocation par image : on lit les pistes en place, et les
                    // colonnes hors de la zone visible ne sont pas dessinées.
                    let mut reset: Option<[Option<u16>; 2]> = None;
                    let mut reset_track: Option<u64> = None;
                    let mut select = None;
                    for idx in 0..self.project.tracks.len() {
                        let (r, resp) =
                            ui.allocate_exact_size(vec2(col_w, area.height()), Sense::click());
                        if !ui.is_rect_visible(r) {
                            continue;
                        }
                        let tr = &self.project.tracks[idx];
                        let (id, armed, color) = (tr.id, tr.armed, track_color(tr, idx));
                        let (gain_db, pan) = (tr.gain_db, tr.pan);
                        let chans: [Option<u16>; 2] = match tr.input {
                            Some(i) if tr.stereo => [Some(i), Some(i + 1)],
                            Some(i) => [Some(i), None],
                            None => [None, None],
                        };
                        let meter = |c: Option<u16>| match c.and_then(|i| i.checked_sub(in_base)) {
                            Some(i) if (i as usize) < audio::MAX_INPUTS && i < in_ch => {
                                self.in_meters[i as usize]
                            }
                            _ => MeterState::default(),
                        };
                        let audio_tr = shows_playback(tr, transport);
                        let (m0, m1, stereo) = if audio_tr {
                            let m = self.track_meters.get(&id).copied().unwrap_or_default();
                            (m[0], m[1], tr.stereo)
                        } else {
                            (meter(chans[0]), meter(chans[1]), chans[1].is_some())
                        };
                        let merged = if stereo { m0.merge(m1) } else { m0 };
                        let p = ui.painter();
                        let sel = self.selected_tracks.contains(&id);
                        if sel || resp.hovered() {
                            p.rect_filled(r, 0.0, if sel { t::SURFACE_3 } else { t::SURFACE_2 });
                        }
                        let pr = Rect::from_center_size(
                            pos2(r.center().x, r.top() + 10.0),
                            vec2(38.0, 15.0),
                        );
                        w::peak_readout(p, pr, &merged);
                        // Panoramique.
                        let pan_r = Rect::from_center_size(
                            pos2(r.center().x, r.top() + 29.0),
                            vec2(col_w - 12.0, 12.0),
                        );
                        if let Some(v) = pan_control(ui, pan_r, pan, ("bpan", id)) {
                            set_pan = Some((idx, v));
                        }
                        // Vumètre à gauche, fader à droite.
                        let mx = r.left() + 16.0;
                        let p = ui.painter();
                        if stereo {
                            for (k, m) in [m0, m1].iter().enumerate() {
                                let x = mx - 4.0 + k as f32 * 4.5;
                                let mr = Rect::from_min_max(pos2(x, top), pos2(x + 3.5, bottom));
                                w::meter_v(p, mr, m);
                            }
                        } else {
                            let mr =
                                Rect::from_min_max(pos2(mx - 4.0, top), pos2(mx + 4.0, bottom));
                            w::meter_v(p, mr, &merged);
                        }
                        let fr = Rect::from_min_max(
                            pos2(r.left() + 26.0, top - 4.0),
                            pos2(r.right() - 4.0, bottom + 4.0),
                        );
                        if let Some(v) = fader(ui, fr, gain_db, ("bfader", id)) {
                            set_gain = Some((idx, v));
                        }
                        let p = ui.painter();
                        p.rect_filled(
                            Rect::from_min_size(
                                pos2(r.left() + 6.0, bottom + 6.0),
                                vec2(col_w - 12.0, 2.0),
                            ),
                            0.0,
                            color,
                        );
                        p.text(
                            pos2(r.left() + 7.0, bottom + 16.0),
                            Align2::LEFT_CENTER,
                            format!("{}", idx + 1),
                            t::semibold(10.0),
                            if armed { t::REC } else { t::TEXT_2 },
                        );
                        p.text(
                            pos2(r.right() - 5.0, bottom + 16.0),
                            Align2::RIGHT_CENTER,
                            fmt_fader(gain_db),
                            t::regular(9.0),
                            t::TEXT_3,
                        );
                        p.text(
                            pos2(r.center().x, bottom + 28.0),
                            Align2::CENTER_CENTER,
                            short(&tr.name, 8),
                            t::regular(9.0),
                            t::TEXT_3,
                        );
                        // Bulle construite seulement au survol.
                        let resp = resp.on_hover_ui(|ui| {
                            let tr = &self.project.tracks[idx];
                            let label = match tr.input {
                                _ if tr.is_audio() => "piste audio".into(),
                                Some(i) if tr.stereo => format!("entrées {}-{}", i + 1, i + 2),
                                Some(i) => format!("entrée {}", i + 1),
                                None => "pas d'entrée".into(),
                            };
                            ui.label(format!("{} — {label}", tr.name));
                        });
                        if resp.clicked()
                            && let Some(pp) = resp.interact_pointer_pos()
                        {
                            if pr.contains(pp) {
                                if audio_tr {
                                    reset_track = Some(id);
                                } else {
                                    reset = Some(chans);
                                }
                            } else {
                                select = Some(id);
                            }
                        }
                    }
                    // Master : niveau envoyé à la sortie, et son fader.
                    ui.add_space(8.0);
                    let (r, _) =
                        ui.allocate_exact_size(vec2(col_w + 8.0, area.height()), Sense::hover());
                    if ui.is_rect_visible(r) {
                        let p = ui.painter();
                        w::vline(p, r.left() + 0.5, r.top(), r.bottom(), t::LINE);
                        let m = self.master_play;
                        let pr = Rect::from_center_size(
                            pos2(r.center().x + 4.0, r.top() + 10.0),
                            vec2(38.0, 15.0),
                        );
                        w::peak_readout(p, pr, &m[0].merge(m[1]));
                        // Clic sur la crête : remise à zéro (comme sur le grand master).
                        reset_master = ui
                            .interact(pr, ui.make_persistent_id("bmaster_pk"), Sense::click())
                            .clicked();
                        let p = ui.painter();
                        let mx = r.left() + 20.0;
                        for (k, mm) in m.iter().enumerate() {
                            let x = mx - 4.0 + k as f32 * 4.5;
                            let mr = Rect::from_min_max(pos2(x, top), pos2(x + 3.5, bottom));
                            w::meter_v(p, mr, mm);
                        }
                        let master_db = self.project.master_db;
                        let fr = Rect::from_min_max(
                            pos2(r.left() + 30.0, top - 4.0),
                            pos2(r.right() - 4.0, bottom + 4.0),
                        );
                        if let Some(v) = fader(ui, fr, master_db, "bmaster") {
                            set_master = Some(v);
                        }
                        let p = ui.painter();
                        p.text(
                            pos2(r.left() + 9.0, bottom + 16.0),
                            Align2::LEFT_CENTER,
                            "M",
                            t::semibold(10.0),
                            t::TEXT_2,
                        );
                        p.text(
                            pos2(r.right() - 5.0, bottom + 16.0),
                            Align2::RIGHT_CENTER,
                            fmt_fader(master_db),
                            t::regular(9.0),
                            t::TEXT_3,
                        );
                        p.text(
                            pos2(r.center().x + 4.0, bottom + 28.0),
                            Align2::CENTER_CENTER,
                            "MASTER",
                            t::caps(9.0),
                            t::TEXT_3,
                        );
                    }
                    if let Some(chans) = reset {
                        for i in chans.into_iter().flatten() {
                            if let Some(mm) = i
                                .checked_sub(in_base)
                                .and_then(|c| self.in_meters.get_mut(c as usize))
                            {
                                mm.reset();
                            }
                        }
                    }
                    if let Some(m) = reset_track.and_then(|id| self.track_meters.get_mut(&id)) {
                        m[0].reset();
                        m[1].reset();
                    }
                    if let Some(id) = select {
                        self.selected_tracks.clear();
                        self.selected_tracks.insert(id);
                    }
                });
            });
        if let Some((idx, v)) = set_gain
            && let Some(tr) = self.project.tracks.get_mut(idx)
        {
            tr.gain_db = v;
            self.mix_changed();
        }
        if let Some((idx, v)) = set_pan
            && let Some(tr) = self.project.tracks.get_mut(idx)
        {
            tr.pan = v;
            self.mix_changed();
        }
        if reset_master {
            for m in &mut self.master_play {
                m.reset();
            }
        }
        if let Some(v) = set_master {
            // Master : appliqué à la sortie dès l'image suivante (`sync_monitoring`).
            self.project.master_db = v;
            self.project.dirty = true;
        }
    }
}

/// Largeur d'une colonne du panneau des vumètres (vumètre + fader).
const BRIDGE_COL_W: f32 = 60.0;

/// Position d'un fader (0 = bas, 1 = haut) pour un gain en dB : course plus fine autour
/// de 0 dB, comme sur une console ; tout en bas = coupé.
fn fader_pos(db: f32) -> f32 {
    use crate::project::{FADER_MAX_DB, FADER_MIN_DB};
    if !db.is_finite() || db <= FADER_MIN_DB {
        return 0.0;
    }
    let x = ((db.max(-60.0) + 60.0) / (FADER_MAX_DB + 60.0)).clamp(0.0, 1.0);
    x.powf(1.6)
}

/// Gain en dB d'une position de fader (inverse de `fader_pos`).
fn fader_db(pos: f32) -> f32 {
    use crate::project::{FADER_MAX_DB, FADER_MIN_DB};
    if pos <= 0.002 {
        return FADER_MIN_DB;
    }
    let db = pos.clamp(0.0, 1.0).powf(1.0 / 1.6) * (FADER_MAX_DB + 60.0) - 60.0;
    // Petit cran à 0 dB.
    if db.abs() < 0.25 {
        0.0
    } else {
        (db * 10.0).round() / 10.0
    }
}

/// Valeur d'un fader, lisible : « 0.0 », « -6.0 », « +3.0 », « -∞ ».
pub fn fmt_fader(db: f32) -> String {
    if !db.is_finite() || db <= crate::project::FADER_MIN_DB {
        "-∞".into()
    } else if db > 0.05 {
        format!("+{db:.1}")
    } else {
        format!("{:.1}", db.min(0.0))
    }
}

/// Fader vertical dans `rect` : glisser pour régler (Maj : réglage fin), double-clic :
/// 0 dB. Renvoie la nouvelle valeur (dB) si elle a changé.
fn fader(ui: &mut egui::Ui, rect: Rect, db: f32, id: impl egui::AsIdSalt) -> Option<f32> {
    let resp = ui.interact(rect, ui.make_persistent_id(id), Sense::click_and_drag());
    let travel = (rect.height() - 8.0).max(1.0);
    let pos = fader_pos(db);
    let mut out = None;
    if resp.double_clicked() {
        out = Some(0.0);
    } else if resp.dragged() {
        // Position suivie depuis le début du geste (pas depuis la valeur arrondie) : le
        // réglage fin (Maj) avance même par petits pas, et quitte le cran de 0 dB.
        let fine = ui.input(|i| i.modifiers.shift);
        let dy = resp.drag_delta().y / travel * if fine { 0.1 } else { 1.0 };
        let key = resp.id.with("pos");
        let start = if resp.drag_started() {
            pos
        } else {
            ui.data(|d| d.get_temp::<f32>(key)).unwrap_or(pos)
        };
        let p = (start - dy).clamp(0.0, 1.0);
        ui.data_mut(|d| d.insert_temp(key, p));
        let v = fader_db(p);
        if v != db {
            out = Some(v);
        }
    }
    let p = ui.painter();
    let cx = rect.center().x;
    let y_of = |pos: f32| rect.bottom() - 4.0 - pos * travel;
    // Rail, repère 0 dB, puis curseur.
    p.rect_filled(
        Rect::from_min_max(
            pos2(cx - 1.0, rect.top() + 4.0),
            pos2(cx + 1.0, rect.bottom() - 4.0),
        ),
        1.0,
        t::INSET,
    );
    let y0 = y_of(fader_pos(0.0));
    p.line_segment(
        [pos2(cx - 6.0, y0), pos2(cx + 6.0, y0)],
        egui::Stroke::new(1.0, t::TEXT_3),
    );
    let y = y_of(fader_pos(out.unwrap_or(db)));
    let cap = Rect::from_center_size(pos2(cx, y), vec2(16.0, 9.0));
    let active = resp.hovered() || resp.dragged();
    p.rect_filled(cap, 2.0, if active { t::ACCENT } else { t::TEXT_2 });
    p.line_segment(
        [pos2(cap.left() + 3.0, y), pos2(cap.right() - 3.0, y)],
        egui::Stroke::new(1.0, t::SURFACE),
    );
    if resp.dragged() || resp.hovered() {
        resp.on_hover_text(fmt_fader(out.unwrap_or(db)));
    }
    out
}

/// Panoramique horizontal dans `rect` : glisser pour régler, double-clic : centre.
/// Renvoie la nouvelle valeur (−1 … 1) si elle a changé.
fn pan_control(ui: &mut egui::Ui, rect: Rect, pan: f32, id: impl egui::AsIdSalt) -> Option<f32> {
    let resp = ui.interact(rect, ui.make_persistent_id(id), Sense::click_and_drag());
    let pan = if pan.is_finite() {
        pan.clamp(-1.0, 1.0)
    } else {
        0.0
    };
    let mut out = None;
    if resp.double_clicked() {
        out = Some(0.0);
    } else if resp.dragged() {
        // Comme le fader : position suivie depuis le début du geste.
        let fine = ui.input(|i| i.modifiers.shift);
        let dx = resp.drag_delta().x / (rect.width() / 2.0) * if fine { 0.1 } else { 1.0 };
        let key = resp.id.with("pan");
        let start = if resp.drag_started() {
            pan
        } else {
            ui.data(|d| d.get_temp::<f32>(key)).unwrap_or(pan)
        };
        let raw = (start + dx).clamp(-1.0, 1.0);
        ui.data_mut(|d| d.insert_temp(key, raw));
        let v = (raw * 100.0).round() / 100.0;
        // Cran au centre.
        let v = if v.abs() < 0.03 { 0.0 } else { v };
        if v != pan {
            out = Some(v);
        }
    }
    let v = out.unwrap_or(pan);
    let p = ui.painter();
    p.rect_filled(rect, 2.0, t::INSET);
    let cx = rect.center().x;
    let x = cx + v * (rect.width() / 2.0 - 2.0);
    let active = resp.hovered() || resp.dragged();
    p.rect_filled(
        Rect::from_min_max(
            pos2(cx.min(x), rect.top() + 2.0),
            pos2(cx.max(x), rect.bottom() - 2.0),
        ),
        1.0,
        if active { t::ACCENT } else { t::ACCENT_DIM },
    );
    p.line_segment(
        [pos2(cx, rect.top()), pos2(cx, rect.bottom())],
        egui::Stroke::new(1.0, t::TEXT_3),
    );
    if active {
        let txt = if v.abs() < 0.005 {
            "C".to_string()
        } else if v < 0.0 {
            format!("G {:.0}", -v * 100.0)
        } else {
            format!("D {:.0}", v * 100.0)
        };
        resp.on_hover_text(txt);
    }
    out
}

/// Rectangle de sélection des prises : point de départ en temps (échantillon) et en
/// hauteur dans la liste des pistes (indépendant du défilement), et sélection d'origine
/// (Ctrl / Maj : on ajoute à la sélection existante).
pub struct Marquee {
    pub sample: u64,
    pub y: f32,
    pub base: HashSet<u64>,
    /// Coin opposé courant (échantillon, hauteur).
    pub cur: (u64, f32),
}

/// Liste déroulante d'entrée (ou de paire en stéréo : 1-2, 3-4…). Les entrées déjà prises
/// par d'autres pistes sont signalées avec le nom de ces pistes. `me` = piste réglée.
/// Retourne `true` si le choix a changé.
#[allow(clippy::too_many_arguments)]
pub fn input_combo(
    ui: &mut egui::Ui,
    id: &str,
    width: f32,
    value: &mut Option<u16>,
    stereo: bool,
    in_ch: u16,
    base: u16,
    tracks: &[crate::project::Track],
    me: Option<u64>,
) -> bool {
    let mut users: std::collections::HashMap<u16, Vec<&str>> = std::collections::HashMap::new();
    for t in tracks.iter().filter(|t| Some(t.id) != me) {
        for c in t.inputs() {
            users.entry(c).or_default().push(t.name.as_str());
        }
    }
    let step: u16 = if stereo { 2 } else { 1 };
    let label = |i: u16| {
        if stereo {
            format!("Entrées {}-{}", i + 1, i + 2)
        } else {
            format!("Entrée {}", i + 1)
        }
    };
    let cur = value.map_or("Aucune entrée".to_string(), label);
    let mut changed = false;
    egui::ComboBox::from_id_salt(id)
        .width(width)
        .height(340.0)
        .selected_text(cur)
        .show_ui(ui, |ui| {
            if ui
                .selectable_label(value.is_none(), "Aucune entrée")
                .clicked()
            {
                *value = None;
                changed = true;
            }
            if in_ch == 0 {
                ui.label(egui::RichText::new("Aucune carte son").color(t::TEXT_3));
            }
            // Numéros réels de la carte : le flux ouvert va de `base` à `base + in_ch`.
            let mut i = base;
            while i + step <= base + in_ch {
                let by: Vec<&str> = (i..i + step)
                    .flat_map(|c| users.get(&c).cloned().unwrap_or_default())
                    .collect();
                let text = if by.is_empty() {
                    egui::RichText::new(label(i))
                } else {
                    egui::RichText::new(format!("{}   · {}", label(i), by.join(", ")))
                        .color(t::TEXT_3)
                };
                if ui.selectable_label(*value == Some(i), text).clicked() {
                    *value = Some(i);
                    changed = true;
                }
                i += step;
            }
        });
    changed
}

/// Largeur du menu de choix d'entrée (8 colonnes).
fn picker_width(stereo: bool) -> f32 {
    8.0 * if stereo { 46.0 } else { 34.0 }
}

/// Grille 8 colonnes pour choisir une entrée (jusqu'à 64 canaux Dante et plus).
/// En stéréo, on choisit une paire fixe : 1-2, 3-4, 5-6…
/// `me` = piste en cours de réglage (ses propres entrées ne comptent pas comme occupées).
fn input_picker(
    ui: &mut egui::Ui,
    current: Option<u16>,
    stereo: bool,
    in_ch: u16,
    base: u16,
    tracks: &[crate::project::Track],
    me: Option<u64>,
) -> Option<Option<u16>> {
    let mut out = None;
    w::section(
        ui,
        if stereo {
            "Paire d'entrées"
        } else {
            "Entrée"
        },
    );
    if in_ch == 0 {
        ui.label(egui::RichText::new("Aucune carte son").color(t::TEXT_3));
    } else if stereo && in_ch < 2 {
        ui.label(egui::RichText::new("La carte n'a qu'une entrée").color(t::TEXT_3));
    }
    let used: HashSet<u16> = tracks
        .iter()
        .filter(|t| Some(t.id) != me)
        .flat_map(|t| t.inputs())
        .collect();
    let (step, cell_w) = if stereo { (2, 44.0) } else { (1, 32.0) };
    let count = if stereo { in_ch / 2 } else { in_ch };
    egui::Grid::new("inpick")
        .spacing([2.0, 2.0])
        .show(ui, |ui| {
            for n in 0..count {
                let i = base + n * step;
                let sel = current == Some(i);
                let is_used = (i..i + step).any(|c| used.contains(&c));
                let (r, resp) = ui.allocate_exact_size(vec2(cell_w, 22.0), Sense::click());
                let bg = if sel {
                    t::ACCENT
                } else if resp.hovered() {
                    t::SURFACE_4
                } else {
                    t::SURFACE_3
                };
                ui.painter().rect_filled(r, t::R, bg);
                let fg = if sel {
                    Color32::BLACK
                } else if is_used {
                    t::TEXT_3
                } else {
                    t::TEXT
                };
                let text = if stereo {
                    format!("{}-{}", i + 1, i + 2)
                } else {
                    format!("{}", i + 1)
                };
                ui.painter()
                    .text(r.center(), Align2::CENTER_CENTER, text, t::medium(10.5), fg);
                if is_used && !sel {
                    ui.painter().rect_filled(
                        Rect::from_min_size(pos2(r.right() - 5.0, r.top() + 2.0), vec2(3.0, 3.0)),
                        0.0,
                        t::TEXT_3,
                    );
                }
                let resp = if is_used {
                    resp.on_hover_text("Déjà utilisée par une autre piste")
                } else {
                    resp
                };
                if resp.clicked() {
                    out = Some(Some(i));
                }
                if n % 8 == 7 {
                    ui.end_row();
                }
            }
        });
    ui.add_space(4.0);
    if ui.button("Aucune entrée").clicked() {
        out = Some(None);
    }
    out
}

/// Prise : bandeau de titre à la couleur de la piste, corps teinté, forme d'onde sombre.
#[allow(clippy::too_many_arguments)]
pub fn draw_clip(
    painter: &egui::Painter,
    rect: Rect,
    visible: Rect,
    color: Color32,
    peaks: Option<&Peaks>,
    offset: u64,
    spp: f64,
    label: Option<&str>,
    selected: bool,
    fades: (u64, u64),
    handles: bool,
) {
    let cr = CornerRadius::same(t::R);
    let body = if selected {
        t::mix(t::LANE_A, color, 0.42)
    } else {
        t::mix(t::LANE_A, color, 0.26)
    };
    painter.rect_filled(rect, cr, body);
    let head = Rect::from_min_max(
        rect.min,
        pos2(rect.right(), (rect.top() + CLIP_HEADER).min(rect.bottom())),
    );
    let head_col = if selected {
        t::mix(color, Color32::WHITE, 0.25)
    } else {
        color
    };
    painter.rect_filled(
        head,
        CornerRadius {
            nw: t::R,
            ne: t::R,
            sw: 0,
            se: 0,
        },
        head_col,
    );
    if let Some(peaks) = peaks {
        let data = peaks.read();
        let l1 = (PEAK_BLOCK * PEAK_L1) as f64;
        let l2 = l1 * PEAK_L1 as f64;
        let (lvl, per, level) = if spp >= l2 * 2.0 {
            (&data.l2, l2, 2u8)
        } else if spp >= l1 * 2.0 {
            (&data.l1, l1, 1)
        } else {
            (&data.l0, PEAK_BLOCK as f64, 0)
        };
        if !lvl.is_empty() {
            // Colonnes repérées depuis le début de la prise (stables quand on fait défiler),
            // calculées une fois puis relues dans le cache.
            let left = rect.left().floor();
            let x0 = left.max(visible.left().floor());
            let x1 = rect.right().min(visible.right()).ceil();
            let k0 = (x0 - left) as i64;
            let wave =
                Rect::from_min_max(pos2(x0, head.bottom() + 2.0), pos2(x1, rect.bottom() - 2.0));
            // Forme claire sur fond sombre teinté : lisible et contrastée.
            let wcol = if selected {
                t::mix(color, Color32::WHITE, 0.55)
            } else {
                t::mix(color, Color32::WHITE, 0.22)
            };
            let id = data.id as usize;
            let mesh = WAVES.with_borrow_mut(|cache| {
                // Paquet courant recopié une fois (256 colonnes), au lieu de deux recherches
                // dans le cache par colonne de pixel à chaque image.
                let mut cur = i64::MIN;
                let mut cols: Vec<Option<(i8, i8)>> = Vec::with_capacity(WAVE_CHUNK as usize);
                w::wave_mesh(wave, wcol, |i| {
                    let k = k0 + i as i64;
                    let ch = k.div_euclid(WAVE_CHUNK);
                    if ch != cur {
                        cols.clear();
                        cols.extend_from_slice(cache.chunk(id, lvl, level, per, offset, spp, ch));
                        cur = ch;
                    }
                    cols.get(k.rem_euclid(WAVE_CHUNK) as usize)
                        .copied()
                        .flatten()
                        .map(|(mn, mx)| (wave_scale(mn), wave_scale(mx)))
                })
            });
            painter.add(egui::Shape::mesh(mesh));
        }
    }
    // Fondus : zone au-dessus de la courbe assombrie, courbe claire, poignées.
    let body_top = head.bottom();
    let x_of = |samples: u64| (samples as f64 / spp) as f32;
    let fi_x = rect.left() + x_of(fades.0);
    let fo_x = rect.right() - x_of(fades.1);
    let h = rect.bottom() - body_top;
    let mut shade = Mesh::default();
    let mut draw_fade = |x0: f32, x1: f32, rising: bool| {
        if x1 - x0 < 1.0 {
            return;
        }
        let mut pts = Vec::new();
        let n = ((x1 - x0) as usize).clamp(2, 400);
        for k in 0..=n {
            let u = k as f32 / n as f32;
            let g = crate::project::s_curve(if rising { u } else { 1.0 - u });
            let x = x0 + (x1 - x0) * u;
            let y = rect.bottom() - g * h;
            pts.push(pos2(x, y));
            if k > 0 && x >= visible.left() && x <= visible.right() {
                shade.add_colored_rect(
                    Rect::from_min_max(pos2(x - (x1 - x0) / n as f32, body_top), pos2(x, y)),
                    Color32::from_black_alpha(120),
                );
            }
        }
        painter.add(egui::Shape::line(
            pts,
            Stroke::new(1.0, Color32::from_white_alpha(170)),
        ));
    };
    if fades.0 > 0 {
        draw_fade(rect.left(), fi_x, true);
    }
    if fades.1 > 0 {
        draw_fade(fo_x, rect.right(), false);
    }
    painter.add(egui::Shape::mesh(shade));
    if handles && rect.width() > 3.0 * EDGE_GRAB {
        for x in [fi_x.max(rect.left() + 3.0), fo_x.min(rect.right() - 3.0)] {
            let r = Rect::from_center_size(pos2(x, head.center().y), vec2(7.0, 7.0));
            painter.rect_filled(r, 1.0, Color32::WHITE);
            painter.rect_stroke(
                r,
                1.0,
                Stroke::new(1.0, Color32::BLACK),
                StrokeKind::Outside,
            );
        }
    }
    if selected {
        painter.rect_stroke(
            rect,
            cr,
            Stroke::new(1.0, Color32::WHITE),
            StrokeKind::Inside,
        );
    } else {
        painter.rect_stroke(
            rect,
            cr,
            Stroke::new(1.0, Color32::from_black_alpha(120)),
            StrokeKind::Inside,
        );
    }
    if let Some(label) = label
        && rect.width() > 40.0
    {
        let clip_rect = head.intersect(visible).shrink2(vec2(4.0, 0.0));
        let pos = pos2(rect.left().max(visible.left()) + 5.0, head.center().y);
        painter.with_clip_rect(clip_rect).text(
            pos,
            Align2::LEFT_CENTER,
            label,
            t::medium(9.5),
            Color32::from_black_alpha(220),
        );
    }
}

fn fmt_ruler(tt: f64, step: f64) -> String {
    let total = tt.round() as u64;
    let (m, s) = (total / 60, total % 60);
    if step < 1.0 {
        // Décimales selon la finesse de la graduation (dixièmes … millièmes).
        let prec = if step < 0.01 {
            3
        } else if step < 0.1 {
            2
        } else {
            1
        };
        format!(
            "{}:{:0width$.prec$}",
            (tt / 60.0).floor() as u64,
            tt % 60.0,
            width = 3 + prec,
            prec = prec
        )
    } else if m >= 60 {
        format!("{}:{:02}:{:02}", m / 60, m % 60, s)
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::fmt_ruler;

    /// En lecture, une piste armée qui a des prises montre ce qu'on entend, pas son entrée.
    #[test]
    fn meters_show_playback_while_playing() {
        use super::shows_playback;
        use crate::app::Transport;
        let mut p = crate::project::Project::new("t".into(), std::env::temp_dir(), 48000);
        let id = p.add_track(None, Some(0));
        let tr = p.tracks.iter_mut().find(|t| t.id == id).unwrap();
        tr.armed = true;
        let play = Transport::Playing { start: 0 };
        let rec = Transport::Recording { start: 0 };
        assert!(!shows_playback(tr, play), "armée sans prise : son entrée");
        tr.clips.push(crate::project::Clip {
            id: 1,
            file: "a.wav".into(),
            start: 0,
            offset: 0,
            len: 48000,
            src_len: 48000,
            fade_in: 0,
            fade_out: 0,
        });
        assert!(shows_playback(tr, play));
        assert!(!shows_playback(tr, rec), "armée en prise : l'entrée enregistrée");
        assert!(!shows_playback(tr, Transport::Stopped));
        tr.armed = false;
        assert!(shows_playback(tr, play));
        assert!(shows_playback(tr, rec));
    }

    /// Prise plus courte que `MIN_CLIP` (clic de 1 ms importé) : bornes du rognage
    /// toujours dans l'ordre, aucune panique ; elle peut seulement être rallongée.
    #[test]
    fn short_clips_trim_without_panic() {
        use super::{trim_left_range, trim_right_range};
        use crate::project::{Clip, MIN_CLIP};
        let c = Clip {
            id: 1,
            file: "clic.wav".into(),
            start: 1000,
            offset: 0,
            len: 48,
            src_len: 48,
            fade_in: 0,
            fade_out: 0,
        };
        let (lo, hi) = trim_left_range(&c);
        assert!(lo <= hi);
        assert_eq!((500i64).clamp(lo, hi), 1000);
        let (lo, hi) = trim_right_range(&c);
        assert!(lo <= hi);
        assert_eq!((900i64).clamp(lo, hi), 1048);
        // Rallongeable à gauche quand le fichier commence avant la prise.
        let c2 = Clip {
            offset: 20,
            src_len: 200,
            ..c.clone()
        };
        assert_eq!(trim_left_range(&c2), (980, 1000));
        assert_eq!(trim_right_range(&c2).1, 1000 + 180);
        // Prise normale : bornes habituelles.
        let n = Clip {
            len: 1000,
            src_len: 1000,
            ..c.clone()
        };
        assert_eq!(trim_left_range(&n), (1000, 2000 - MIN_CLIP as i64));
        assert_eq!(trim_right_range(&n), (1000 + MIN_CLIP as i64, 2000));
        // Fichier source abîmé (longueur inférieure au décalage) : pas de débordement.
        let bad = Clip {
            offset: 500,
            src_len: 10,
            ..n.clone()
        };
        let (lo, hi) = trim_right_range(&bad);
        assert!(lo <= hi);
    }

    /// Cache des formes d'onde : mêmes valeurs qu'un calcul direct, recalcul quand une
    /// prise en cours s'allonge, et troisième niveau de détail tenu à jour.
    #[test]
    fn wave_cache_matches_direct_computation() {
        use super::WaveCache;
        use crate::render::{PEAK_BLOCK, PEAK_L1};
        let mut pd = crate::render::PeakData::default();
        let pts: Vec<crate::render::Peak> = (0..5000)
            .map(|i| [-((i % 120) as i8), (i % 127) as i8])
            .collect();
        pd.extend(&pts[..3000]);
        assert_eq!(pd.l2.len(), 3000_usize.div_ceil(PEAK_L1 * PEAK_L1));
        let (per, spp, offset) = (PEAK_BLOCK as f64, 3.5 * PEAK_BLOCK as f64, 2048);
        let direct = |lvl: &[crate::render::Peak], k: i64| {
            let s0 = offset as f64 + k as f64 * spp;
            let i0 = (s0 / per) as usize;
            let i1 = (((s0 + spp) / per) as usize).max(i0 + 1).min(lvl.len());
            (i0 < i1).then(|| {
                lvl[i0..i1]
                    .iter()
                    .fold((i8::MAX, i8::MIN), |(a, b), p| (a.min(p[0]), b.max(p[1])))
            })
        };
        let mut c = WaveCache::default();
        for k in [0, 1, 255, 256, 700, 857, 900] {
            assert_eq!(
                c.column(1, &pd.l0, 0, per, offset, spp, k),
                direct(&pd.l0, k),
                "colonne {k}"
            );
        }
        // La prise s'allonge : le paquet incomplet est recalculé.
        assert_eq!(c.column(1, &pd.l0, 0, per, offset, spp, 900), None);
        pd.extend(&pts[3000..]);
        assert_eq!(
            c.column(1, &pd.l0, 0, per, offset, spp, 900),
            direct(&pd.l0, 900)
        );
        assert!(c.column(1, &pd.l0, 0, per, offset, spp, 900).is_some());
    }

    /// Charge : forme d'onde d'une prise de 3 h (48 kHz) : mémoire des points, calcul de
    /// toute la largeur d'écran au zoom arrière, puis défilement (relecture du cache).
    /// `cargo test --release -- --ignored --nocapture charge_formes_d_onde`
    #[test]
    #[ignore]
    fn charge_formes_d_onde_3h() {
        use super::WaveCache;
        use crate::render::{PEAK_BLOCK, PEAK_L1, PeakData};
        let frames = 3 * 3600 * 48_000usize;
        let n = frames.div_ceil(PEAK_BLOCK);
        let pts: Vec<crate::render::Peak> = (0..n)
            .map(|i| [-((i % 97) as i8), (i % 109) as i8])
            .collect();
        let t = std::time::Instant::now();
        let mut pd = PeakData::default();
        pd.extend(&pts);
        let build = t.elapsed();
        let mem = (pd.l0.len() + pd.l1.len() + pd.l2.len()) * 4;
        let width = 1600i64;
        for (label, spp) in [
            ("zoom arrière (3 h à l'écran)", frames as f64 / width as f64),
            (
                "zoom moyen (1 min à l'écran)",
                48_000.0 * 60.0 / width as f64,
            ),
            ("zoom fin (1 s à l'écran)", 48_000.0 / width as f64),
        ] {
            let l1 = (PEAK_BLOCK * PEAK_L1) as f64;
            let l2 = l1 * PEAK_L1 as f64;
            let (lvl, per, level) = if spp >= l2 * 2.0 {
                (&pd.l2, l2, 2u8)
            } else if spp >= l1 * 2.0 {
                (&pd.l1, l1, 1)
            } else {
                (&pd.l0, PEAK_BLOCK as f64, 0)
            };
            let mut c = WaveCache::default();
            let t = std::time::Instant::now();
            for k in 0..width {
                std::hint::black_box(c.column(1, lvl, level, per, 0, spp, k));
            }
            let cold = t.elapsed();
            // Défilement de 3 px par image pendant 60 images : relu dans le cache.
            let t = std::time::Instant::now();
            for f in 0..60i64 {
                for k in 0..width {
                    std::hint::black_box(c.column(1, lvl, level, per, 0, spp, k + f * 3));
                }
            }
            let warm = t.elapsed() / 60;
            println!(
                "formes d'onde · {label} : 1re image {cold:?}, image suivante {warm:?} (piste de 3 h, {:.1} Mo de points, construits en {build:?})",
                mem as f64 / 1e6
            );
            assert!(
                warm < std::time::Duration::from_millis(4),
                "dessin trop lent"
            );
        }
    }

    /// Graduations : décimales selon la finesse (zoom), jusqu'à la milliseconde.
    #[test]
    fn ruler_labels_follow_zoom() {
        assert_eq!(fmt_ruler(65.5, 0.5), "1:05.5");
        assert_eq!(fmt_ruler(1.25, 0.05), "0:01.25");
        assert_eq!(fmt_ruler(1.125, 0.005), "0:01.125");
        assert_eq!(fmt_ruler(125.0, 5.0), "2:05");
    }
}

#[cfg(test)]
mod fader_tests {
    use super::*;

    /// Course du fader : aller-retour position ↔ dB, cran à 0 dB, tout en bas = coupé.
    #[test]
    fn fader_travel_round_trips() {
        use crate::project::{FADER_MAX_DB, FADER_MIN_DB};
        assert_eq!(fader_pos(FADER_MIN_DB), 0.0);
        assert_eq!(fader_db(0.0), FADER_MIN_DB);
        assert!((fader_pos(FADER_MAX_DB) - 1.0).abs() < 1e-6);
        for db in [-40.0f32, -12.0, -6.0, -3.0, 0.0, 3.0, 6.0] {
            let back = fader_db(fader_pos(db));
            assert!((back - db).abs() < 0.11, "{db} → {back}");
        }
        assert_eq!(fader_db(fader_pos(0.2)), 0.0, "cran à 0 dB");
        assert!(fader_pos(0.0) > 0.8 && fader_pos(0.0) < 0.9);
        assert_eq!(fmt_fader(FADER_MIN_DB), "-∞");
        assert_eq!(fmt_fader(0.0), "0.0");
        assert_eq!(fmt_fader(-6.0), "-6.0");
        assert_eq!(fmt_fader(3.0), "+3.0");
    }
}
