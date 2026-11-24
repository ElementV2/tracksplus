//! Onglet Multipiste : barre d'outils, pistes, timeline, montage, vumètres.

use super::theme as t;
use super::widgets::{self as w, BtnStyle, Icon, MeterState};
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

/// Panneau de vumètre master stéréo (partagé avec le soundboard).
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
    let rw = 32.0;
    let top = full.top() + 26.0;
    let mut reset = false;
    for (k, m) in meters.iter().enumerate() {
        let x = full.center().x + if k == 0 { -rw - 1.0 } else { 1.0 };
        let r = Rect::from_min_size(pos2(x, top), vec2(rw, 16.0));
        w::peak_readout(&p, r, m);
        if ui
            .interact(r, ui.make_persistent_id((title, "pk", k)), Sense::click())
            .on_hover_text("Crête max — clic : remise à zéro")
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
    let bw = 10.0;
    let gap = 22.0;
    let lx = full.center().x - gap / 2.0 - bw;
    let rx = full.center().x + gap / 2.0;
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
    let sc = Rect::from_min_max(
        pos2(full.center().x - 8.0, mtop),
        pos2(full.center().x + 8.0, mbot),
    );
    for db in w::SCALE {
        let y = sc.bottom() - w::deflection(db) * sc.height();
        w::hline(&p, lx + bw, lx + bw + 3.0, y, t::TEXT_3);
        w::hline(&p, rx - 3.0, rx, y, t::TEXT_3);
    }
    let mut last = f32::MAX;
    for db in w::SCALE.iter().rev() {
        let y = sc.bottom() - w::deflection(*db) * sc.height();
        if last - y < 11.0 && *db != 0.0 {
            continue;
        }
        last = y;
        p.text(
            pos2(sc.center().x, y),
            Align2::CENTER_CENTER,
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
            .exact_size(36.0)
            .frame(
                egui::Frame::new()
                    .fill(t::SURFACE_2)
                    .inner_margin(egui::Margin::symmetric(10, 0)),
            )
            .show(ui, |ui| self.toolbar(ui));
        egui::Panel::right("master")
            .exact_size(86.0)
            .frame(egui::Frame::new().fill(t::SURFACE))
            .show(ui, |ui| {
                let mut m = self.master_play;
                master_panel(ui, "MASTER", &mut m);
                self.master_play = m;
            });
        if self.show_bridge {
            egui::Panel::bottom("bridge")
                .resizable(true)
                .default_size(210.0)
                .min_size(140.0)
                .max_size(420.0)
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
            self.selection = None;
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
        let recording = self.is_recording();
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            if w::tool_button(ui, Some(Icon::Follow), "Suivre", self.view.follow, true)
                .on_hover_text("La timeline suit la tête de lecture")
                .clicked()
            {
                self.view.follow = !self.view.follow;
                self.view.nav_until = 0.0;
            }
            if w::tool_button(ui, Some(Icon::Snap), "Magnétisme", self.snap, true)
                .on_hover_text("Les prises s'aimantent à la tête, aux repères et aux autres prises (Maj pour désactiver)")
                .clicked()
            {
                self.snap = !self.snap;
            }
            ui.add_space(8.0);
            if w::tool_button(ui, Some(Icon::Marker), "Repère", false, true)
                .on_hover_text("Ajouter un repère à la tête de lecture (M), même en enregistrement")
                .clicked()
            {
                self.add_marker();
            }
            if w::tool_button(ui, Some(Icon::Split), "Couper", false, !recording)
                .on_hover_text("Couper à la tête de lecture (S) : prises sélectionnées, ou toutes celles sous la tête")
                .clicked()
            {
                self.split_at_playhead();
            }
            let can_del = !recording && (!self.sel_clips.is_empty() || self.selection.is_some());
            if w::tool_button(ui, Some(Icon::Close), "Supprimer", false, can_del)
                .on_hover_text("Supprimer les prises sélectionnées, ou le contenu de la plage sélectionnée (Suppr)")
                .clicked()
            {
                self.delete_selection();
            }
            ui.add_space(8.0);
            if w::icon_button(ui, Icon::Undo, 24.0, BtnStyle::Ghost, !self.undo.is_empty() && !recording, "Annuler (Ctrl+Z)").clicked() {
                self.undo();
            }
            let (r, resp) = ui.allocate_exact_size(vec2(24.0, 24.0), Sense::click());
            let en = !self.redo.is_empty() && !recording;
            if resp.hovered() && en {
                ui.painter().rect_filled(r, t::R, t::SURFACE_3);
            }
            // Icône « rétablir » = annuler en miroir.
            let col = if en { t::TEXT_2 } else { t::TEXT_3.gamma_multiply(0.5) };
            let c = r.center();
            let s = 5.8;
            let pts: Vec<egui::Pos2> = (0..=10)
                .map(|k| {
                    let a = std::f32::consts::FRAC_PI_2 + std::f32::consts::PI * k as f32 / 10.0;
                    c + vec2(0.55 * s * a.cos() - 0.1 * s, 0.55 * s * a.sin() + 0.1 * s)
                })
                .collect();
            ui.painter().add(egui::Shape::line(pts, Stroke::new(1.0, col)));
            let tip = c + vec2(-0.1 * s, -0.45 * s);
            ui.painter().add(egui::Shape::convex_polygon(
                vec![tip + vec2(0.45, 0.0) * s, tip + vec2(0.0, -0.3) * s, tip + vec2(0.0, 0.3) * s],
                col,
                Stroke::NONE,
            ));
            if resp.on_hover_text("Rétablir (Ctrl+Maj+Z)").clicked() && en {
                self.redo();
            }
            ui.add_space(8.0);
            if w::tool_button(ui, Some(Icon::Meters), "Vumètres", self.show_bridge, true)
                .on_hover_text("Afficher tous les vumètres d'entrée")
                .clicked()
            {
                self.show_bridge = !self.show_bridge;
            }
            if self.selection.is_some()
                && w::tool_button(ui, Some(Icon::Close), "Effacer la plage", false, true)
                    .on_hover_text("Désélectionner la plage de temps (Échap)")
                    .clicked()
            {
                self.selection = None;
            }
        });

        let right = Rect::from_min_max(pos2(full.right() - 420.0, full.top()), full.max);
        let mut child = ui.new_child(
            UiBuilder::new()
                .max_rect(right)
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
        );
        child.spacing_mut().item_spacing.x = 4.0;
        if w::tool_button(
            &mut child,
            Some(Icon::Export),
            "Exporter",
            false,
            self.project.has_audio() && stopped,
        )
        .clicked()
        {
            self.open_export();
        }
        let in_ch = self.engine.status.in_channels;
        if w::tool_button(
            &mut child,
            None,
            &format!("Une piste par entrée ({in_ch})"),
            false,
            stopped && in_ch > 0,
        )
        .on_hover_text("Crée une piste pour chaque entrée non utilisée (idéal pour Dante)")
        .clicked()
        {
            self.add_track_per_input();
        }
        if w::tool_button(&mut child, Some(Icon::Plus), "Piste", false, stopped).clicked() {
            self.add_track();
        }
    }

    // ---------------------------------------------------------------- repères temporels

    fn x_of(&self, left: f32, sample: u64) -> f32 {
        left + ((sample as f64 - self.view.scroll) / self.view.spp) as f32
    }

    fn sample_at(&self, left: f32, x: f32) -> u64 {
        (self.view.scroll + (x - left) as f64 * self.view.spp).max(0.0) as u64
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
        let steps = [
            0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 900.0,
            1800.0, 3600.0,
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
            }
        }

        let mut marker_hit = None;
        for m in &self.project.markers {
            let x = self.x_of(rect.left(), m.pos);
            if x < rect.left() - 120.0 || x > rect.right() {
                continue;
            }
            let gal = p.layout_no_wrap(m.name.clone(), t::medium(9.5), Color32::BLACK);
            let r = Rect::from_min_size(pos2(x, rect.top() + 2.0), vec2(gal.size().x + 8.0, 13.0));
            p.rect_filled(r, CornerRadius::ZERO, t::MARKER);
            p.galley(
                pos2(r.left() + 4.0, r.center().y - gal.size().y / 2.0),
                gal,
                Color32::BLACK,
            );
            w::vline(&p, x, rect.top() + 2.0, rect.bottom(), t::MARKER);
            if ui.rect_contains_pointer(r) {
                marker_hit = Some((m.id, r));
            }
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
        if let Some((id, r)) = marker_hit {
            let mresp = ui.interact(r, ui.make_persistent_id(("marker", id)), Sense::click());
            if mresp.clicked()
                && let Some(m) = self.project.markers.iter().find(|m| m.id == id)
            {
                let pos = m.pos;
                self.seek(pos);
            }
            mresp.context_menu(|ui| self.marker_menu(ui, id));
            return;
        }
        self.time_select(&resp, rect.left());
    }

    fn marker_menu(&mut self, ui: &mut egui::Ui, id: u64) {
        let Some(idx) = self.project.markers.iter().position(|m| m.id == id) else {
            return;
        };
        ui.set_width(220.0);
        if ui
            .add(
                egui::TextEdit::singleline(&mut self.project.markers[idx].name)
                    .desired_width(200.0),
            )
            .changed()
        {
            self.project.dirty = true;
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

    /// Clic = déplacer la tête ; glisser = sélection de temps.
    fn time_select(&mut self, resp: &egui::Response, left: f32) {
        if resp.drag_started() {
            let origin = resp.ctx.input(|i| i.pointer.press_origin());
            if let Some(p) = origin.or(resp.interact_pointer_pos()) {
                self.ruler_anchor = Some(self.sample_at(left, p.x));
            }
        }
        if resp.dragged()
            && let (Some(a), Some(p)) = (self.ruler_anchor, resp.interact_pointer_pos())
        {
            let b = self.sample_at(left, p.x);
            if (b as f64 - a as f64).abs() / self.view.spp > 3.0 {
                self.selection = Some((a.min(b), a.max(b)));
            }
        }
        if resp.drag_stopped() {
            self.ruler_anchor = None;
            if let Some((a, _)) = self.selection
                && self.is_stopped()
            {
                self.playhead = a;
            }
        }
        if resp.clicked()
            && !self.is_recording()
            && let Some(p) = resp.interact_pointer_pos()
        {
            let s = self.sample_at(left, p.x);
            self.seek(s);
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

    /// Décalage à appliquer pour aimanter l'une des positions `edges` (déjà décalées).
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
        best.unwrap_or(0)
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

    fn update_clip_drag(&mut self, sample: u64, target_row: Option<usize>, no_snap: bool) {
        let Some(drag) = self.clip_drag.take() else {
            return;
        };
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
                    let lo = (m.start as i64 - m.offset as i64).max(0);
                    let hi = m.end() as i64 - MIN_CLIP as i64;
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
                    let lo = m.start as i64 + MIN_CLIP as i64;
                    let hi = m.start as i64 + (m.src_end() - m.offset) as i64;
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

    fn end_clip_drag(&mut self) {
        let Some(drag) = self.clip_drag.take() else {
            return;
        };
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
        let editable = !self.is_recording();
        let in_ch = self.engine.status.in_channels;
        let blink = (self.now * 1.6).fract() < 0.55;
        let (pointer, press_origin, shift, command) = ui.input(|i| {
            (
                i.pointer.hover_pos(),
                i.pointer.press_origin(),
                i.modifiers.shift,
                i.modifiers.command,
            )
        });

        // Grille verticale (repères de la règle).
        let step = self.ruler_step();
        let sr = self.sr() as f64;
        let grid_x: Vec<f32> = {
            let t0 = self.view.scroll / sr;
            let t1 = t0 + tl_width as f64 * self.view.spp / sr;
            let mut v = Vec::new();
            let mut tt = (t0 / step).floor() * step;
            while tt <= t1 {
                v.push(self.x_of(tl_left, (tt * sr) as u64));
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
            if ntracks == 0 {
                ui.add_space(40.0);
                ui.vertical_centered(|ui| {
                    ui.label(
                        egui::RichText::new("Aucune piste")
                            .font(t::semibold(13.0))
                            .color(t::TEXT_2),
                    );
                    ui.label(
                        egui::RichText::new(
                            "Ajoutez une piste ou créez-en une par entrée de la carte.",
                        )
                        .color(t::TEXT_3),
                    );
                });
            }
            for idx in 0..ntracks {
                let (row, _) =
                    ui.allocate_exact_size(vec2(ui.available_width(), track_h), Sense::hover());
                let tid = self.project.tracks[idx].id;
                let dragging_here = self
                    .clip_drag
                    .as_ref()
                    .is_some_and(|d| d.origs.iter().any(|(t, _)| *t == tid));
                if !ui.is_rect_visible(row) && !dragging_here {
                    continue;
                }
                let color = t::color(idx);
                let header = Rect::from_min_size(row.min, vec2(HEADER_W, track_h));
                let lane = Rect::from_min_max(pos2(row.left() + HEADER_W, row.top()), row.max);
                let selected = self.selected_tracks.contains(&tid);
                let rec_live = self.live.iter().any(|l| l.track_id == tid);
                let p = ui.painter().clone();
                p.rect_filled(lane, 0.0, if idx % 2 == 0 { t::LANE_A } else { t::LANE_B });
                let lp = ui.painter_at(lane);
                for &gx in &grid_x {
                    w::vline(&lp, gx, lane.top(), lane.bottom(), t::GRID);
                }
                p.rect_filled(
                    header,
                    0.0,
                    if selected { t::SURFACE_3 } else { t::SURFACE },
                );
                p.rect_filled(
                    Rect::from_min_size(header.min, vec2(3.0, track_h)),
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
                hresp.context_menu(|ui| {
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
                let track = &mut self.project.tracks[idx];
                hdr.horizontal(|ui| {
                    if w::arm_button(
                        ui,
                        track.armed,
                        stopped && track.input.is_some(),
                        !rec_live || blink,
                    )
                    .clicked()
                    {
                        track.armed = !track.armed;
                        self.project.dirty = true;
                    }
                    ui.add_space(2.0);
                    let r = ui.add_enabled(
                        stopped,
                        egui::TextEdit::singleline(&mut track.name)
                            .desired_width(178.0)
                            .font(t::medium(12.0))
                            .frame(egui::Frame::NONE)
                            .margin(egui::Margin::symmetric(2, 2)),
                    );
                    if r.changed() {
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
                let input = track.input;
                let mut new_input: Option<Option<u16>> = None;
                hdr.horizontal(|ui| {
                    let label = match input {
                        Some(i) => format!("IN {}", i + 1),
                        None => "IN —".into(),
                    };
                    let (r, resp) = ui.allocate_exact_size(
                        vec2(48.0, 18.0),
                        if stopped {
                            Sense::click()
                        } else {
                            Sense::hover()
                        },
                    );
                    let hov = resp.hovered() && stopped;
                    ui.painter()
                        .rect_filled(r, t::R, if hov { t::SURFACE_4 } else { t::INSET });
                    ui.painter().text(
                        r.center(),
                        Align2::CENTER_CENTER,
                        label,
                        t::medium(10.0),
                        if input.is_some() { t::TEXT } else { t::TEXT_3 },
                    );
                    let resp = resp.on_hover_text("Choisir l'entrée de la carte");
                    egui::Popup::menu(&resp).width(8.0 * 34.0).show(|ui| {
                        new_input = input_picker(ui, input, in_ch, &self.project.tracks, tid);
                    });
                    let m = input
                        .filter(|&i| (i as usize) < audio::MAX_INPUTS && i < in_ch)
                        .map(|i| self.in_meters[i as usize])
                        .unwrap_or_default();
                    let (mr, _) = ui.allocate_exact_size(vec2(162.0, 18.0), Sense::hover());
                    w::meter_h(
                        ui.painter(),
                        Rect::from_center_size(mr.center(), vec2(mr.width(), 6.0)),
                        &m,
                    );
                    let (pr, presp) = ui.allocate_exact_size(vec2(36.0, 16.0), Sense::click());
                    w::peak_readout(ui.painter(), pr, &m);
                    if presp
                        .on_hover_text("Crête max — clic : remise à zéro")
                        .clicked()
                        && let Some(i) = input
                        && let Some(mm) = self.in_meters.get_mut(i as usize)
                    {
                        mm.reset();
                    }
                });
                if let Some(ni) = new_input {
                    let track = &mut self.project.tracks[idx];
                    track.input = ni;
                    if ni.is_none() {
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
                if let Transport::Recording { start } = self.transport
                    && let Some(l) = self.live.iter().find(|l| l.track_id == track.id)
                {
                    let x0 = self.x_of(lane.left(), start);
                    let x1 = self.x_of(lane.left(), self.playhead);
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
                            self.ruler_anchor = Some(s);
                        }
                    }
                }
                if resp.dragged() {
                    if self.clip_drag.is_some() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
                        if let Some(pp) = resp.interact_pointer_pos() {
                            let s = self.sample_at(lane.left(), pp.x);
                            let row_f = (pp.y - lanes_top) / track_h;
                            let target = (row_f >= 0.0).then_some(row_f as usize);
                            self.update_clip_drag(s, target, shift);
                        }
                    } else if let (Some(a), Some(pp)) =
                        (self.ruler_anchor, resp.interact_pointer_pos())
                    {
                        let b = self.sample_at(lane.left(), pp.x);
                        if (b as f64 - a as f64).abs() / self.view.spp > 3.0 {
                            self.selection = Some((a.min(b), a.max(b)));
                        }
                    }
                }
                if resp.drag_stopped() {
                    if self.clip_drag.is_some() {
                        self.end_clip_drag();
                    } else {
                        self.ruler_anchor = None;
                        if let (Some((a, _)), true) = (self.selection, self.is_stopped()) {
                            self.playhead = a;
                        }
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
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                if w::tool_button(ui, Some(Icon::Plus), "Ajouter une piste", false, stopped)
                    .clicked()
                {
                    self.add_track();
                }
            });
            ui.add_space(40.0);
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
        if clip_to_delete.is_some() {
            self.delete_selection();
        }
    }

    // ---------------------------------------------------------------- bandeau de vumètres

    fn meter_bridge(&mut self, ui: &mut egui::Ui) {
        let full = ui.max_rect();
        let in_ch = self.engine.status.in_channels;
        let p = ui.painter().clone();
        w::hline(&p, full.left(), full.right(), full.top(), t::LINE);
        let top = full.top() + 44.0;
        let bottom = full.bottom() - 34.0;
        p.text(
            pos2(full.left() + 12.0, full.top() + 12.0),
            Align2::LEFT_CENTER,
            "ENTRÉES",
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
        egui::ScrollArea::horizontal()
            .id_salt("bridge")
            .auto_shrink([false, false])
            .show(&mut child, |ui| {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 1.0;
                    let col_w = 44.0;
                    let tracks: Vec<(u64, String, Option<u16>, bool, usize)> = self
                        .project
                        .tracks
                        .iter()
                        .enumerate()
                        .map(|(i, tr)| (tr.id, tr.name.clone(), tr.input, tr.armed, i))
                        .collect();
                    for (id, name, input, armed, idx) in tracks {
                        let (r, resp) =
                            ui.allocate_exact_size(vec2(col_w, area.height()), Sense::click());
                        let p = ui.painter();
                        let sel = self.selected_tracks.contains(&id);
                        if sel || resp.hovered() {
                            p.rect_filled(r, 0.0, if sel { t::SURFACE_3 } else { t::SURFACE_2 });
                        }
                        let m = input
                            .filter(|&i| (i as usize) < audio::MAX_INPUTS && i < in_ch)
                            .map(|i| self.in_meters[i as usize])
                            .unwrap_or_default();
                        let pr = Rect::from_center_size(
                            pos2(r.center().x, r.top() + 10.0),
                            vec2(38.0, 15.0),
                        );
                        w::peak_readout(p, pr, &m);
                        let mr = Rect::from_min_max(
                            pos2(r.center().x - 4.0, top),
                            pos2(r.center().x + 4.0, bottom),
                        );
                        w::meter_v(p, mr, &m);
                        p.rect_filled(
                            Rect::from_min_size(
                                pos2(r.left() + 6.0, bottom + 6.0),
                                vec2(col_w - 12.0, 2.0),
                            ),
                            0.0,
                            t::color(idx),
                        );
                        p.text(
                            pos2(r.center().x, bottom + 16.0),
                            Align2::CENTER_CENTER,
                            format!("{}", idx + 1),
                            t::semibold(10.0),
                            if armed { t::REC } else { t::TEXT_2 },
                        );
                        p.text(
                            pos2(r.center().x, bottom + 28.0),
                            Align2::CENTER_CENTER,
                            short(&name, 6),
                            t::regular(9.0),
                            t::TEXT_3,
                        );
                        let resp = resp.on_hover_text(format!(
                            "{name} — {}",
                            input.map_or("pas d'entrée".to_string(), |i| format!(
                                "entrée {}",
                                i + 1
                            ))
                        ));
                        if resp.clicked()
                            && let Some(pp) = resp.interact_pointer_pos()
                        {
                            if pr.contains(pp) {
                                if let Some(i) = input
                                    && let Some(mm) = self.in_meters.get_mut(i as usize)
                                {
                                    mm.reset();
                                }
                            } else {
                                self.selected_tracks.clear();
                                self.selected_tracks.insert(id);
                            }
                        }
                    }
                });
            });
    }
}

/// Grille 8 colonnes pour choisir une entrée (jusqu'à 64 canaux Dante et plus).
fn input_picker(
    ui: &mut egui::Ui,
    current: Option<u16>,
    in_ch: u16,
    tracks: &[crate::project::Track],
    me: u64,
) -> Option<Option<u16>> {
    let mut out = None;
    w::section(ui, "Entrée");
    if in_ch == 0 {
        ui.label(egui::RichText::new("Aucune carte son").color(t::TEXT_3));
    }
    let used: Vec<u16> = tracks
        .iter()
        .filter(|t| t.id != me)
        .filter_map(|t| t.input)
        .collect();
    egui::Grid::new("inpick")
        .spacing([2.0, 2.0])
        .show(ui, |ui| {
            for i in 0..in_ch {
                let sel = current == Some(i);
                let is_used = used.contains(&i);
                let (r, resp) = ui.allocate_exact_size(vec2(32.0, 22.0), Sense::click());
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
                ui.painter().text(
                    r.center(),
                    Align2::CENTER_CENTER,
                    format!("{}", i + 1),
                    t::medium(10.5),
                    fg,
                );
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
                if i % 8 == 7 {
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
        t::mix(t::LANE_A, color, 0.62)
    } else {
        t::mix(t::LANE_A, color, 0.40)
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
        let (lvl, per) = if spp >= (PEAK_BLOCK * PEAK_L1 * 2) as f64 {
            (&data.l1, (PEAK_BLOCK * PEAK_L1) as f64)
        } else {
            (&data.l0, PEAK_BLOCK as f64)
        };
        if !lvl.is_empty() {
            let x0 = rect.left().max(visible.left()).floor();
            let x1 = rect.right().min(visible.right()).ceil();
            let wave =
                Rect::from_min_max(pos2(x0, head.bottom() + 2.0), pos2(x1, rect.bottom() - 2.0));
            let wcol = t::mix(body, Color32::BLACK, 0.62);
            let mesh = w::wave_mesh(wave, wcol, |i| {
                let x = x0 + i as f32;
                let s0 = offset as f64 + (x - rect.left()) as f64 * spp;
                let i0 = (s0 / per) as usize;
                let i1 = (((s0 + spp) / per) as usize).max(i0 + 1).min(lvl.len());
                if i0 >= lvl.len() {
                    return None;
                }
                let (mut mn, mut mx) = (i16::MAX, i16::MIN);
                for pk in &lvl[i0..i1] {
                    mn = mn.min(pk[0]);
                    mx = mx.max(pk[1]);
                }
                Some((mn as f32 / 32767.0, mx as f32 / 32767.0))
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
        format!("{}:{:04.1}", (tt / 60.0).floor() as u64, tt % 60.0)
    } else if m >= 60 {
        format!("{}:{:02}:{:02}", m / 60, m % 60, s)
    } else {
        format!("{m}:{s:02}")
    }
}
