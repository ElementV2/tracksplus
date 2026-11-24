//! Onglet Multipiste : transport, pistes, timeline, vumètres.

use super::theme as t;
use super::widgets::{self as w, BtnStyle, Icon, MeterState, sq};
use crate::app::{App, Transport, fmt_time, fmt_time_short, short};
use crate::audio;
use crate::render::{PEAK_BLOCK, PEAK_L1, Peaks};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, Key, Pos2, Rect, Sense, Stroke, StrokeKind, UiBuilder,
    pos2, vec2,
};

const HEADER_W: f32 = 304.0;
const RULER_H: f32 = 30.0;
const SCROLLBAR_H: f32 = 14.0;

/// Panneau de vumètre master stéréo (partagé avec le soundboard).
pub fn master_panel(ui: &mut egui::Ui, title: &str, meters: &mut [MeterState; 2]) {
    let full = ui.max_rect();
    let p = ui.painter().clone();
    p.text(
        pos2(full.center().x, full.top() + 16.0),
        Align2::CENTER_CENTER,
        title,
        t::semibold(10.5),
        t::TEXT_3,
    );
    // Valeurs crête (clic = remise à zéro).
    let rw = 34.0;
    let top = full.top() + 32.0;
    let mut reset = false;
    for (k, m) in meters.iter().enumerate() {
        let x = full.center().x + if k == 0 { -rw - 2.0 } else { 2.0 };
        let r = Rect::from_min_size(pos2(x, top), vec2(rw, 18.0));
        w::peak_readout(&p, r, m);
        if ui
            .interact(r, ui.make_persistent_id((title, "pk", k)), Sense::click())
            .on_hover_text("Remettre à zéro")
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
    let mtop = top + 30.0;
    let mbot = full.bottom() - 26.0;
    let bw = 12.0;
    let gap = 26.0;
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
    w::scale_v(
        &p,
        Rect::from_min_max(
            pos2(full.center().x - 10.0, mtop),
            pos2(full.center().x + 10.0, mbot),
        ),
        false,
    );
    // Recentrer les chiffres de graduation.
    p.text(
        pos2(lx + bw / 2.0, mbot + 12.0),
        Align2::CENTER_CENTER,
        "L",
        t::medium(10.0),
        t::TEXT_3,
    );
    p.text(
        pos2(rx + bw / 2.0, mbot + 12.0),
        Align2::CENTER_CENTER,
        "R",
        t::medium(10.0),
        t::TEXT_3,
    );
}

impl App {
    pub fn multitrack_ui(&mut self, ui: &mut egui::Ui) {
        self.multitrack_keys(ui.ctx());
        egui::Panel::top("transport")
            .exact_size(74.0)
            .frame(
                egui::Frame::new()
                    .fill(t::BG)
                    .inner_margin(egui::Margin::symmetric(16, 0)),
            )
            .show(ui, |ui| self.transport_bar(ui));
        egui::Panel::right("master")
            .exact_size(104.0)
            .frame(
                egui::Frame::new()
                    .fill(t::SURFACE)
                    .inner_margin(egui::Margin::same(0)),
            )
            .show(ui, |ui| {
                let mut m = self.master_play;
                master_panel(ui, "MASTER", &mut m);
                self.master_play = m;
            });
        if self.show_bridge {
            egui::Panel::bottom("bridge")
                .resizable(true)
                .default_size(230.0)
                .min_size(150.0)
                .max_size(420.0)
                .frame(
                    egui::Frame::new()
                        .fill(t::SURFACE)
                        .inner_margin(egui::Margin::symmetric(0, 0)),
                )
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
        let (space, rec, home, end, marker, fit, esc, all, left, right) = ctx.input_mut(|i| {
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
            )
        });
        if space {
            if self.is_stopped() {
                self.play();
            } else {
                self.stop();
            }
        }
        if rec {
            if self.is_recording() {
                self.stop();
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
        }
        if all {
            self.selected_tracks = self.project.tracks.iter().map(|t| t.id).collect();
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

    // ---------------------------------------------------------------- transport

    fn transport_bar(&mut self, ui: &mut egui::Ui) {
        let recording = self.is_recording();
        let playing = matches!(self.transport, Transport::Playing { .. });
        let stopped = self.is_stopped();
        let full = ui.max_rect();
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            // Groupe transport
            egui::Frame::new()
                .fill(t::SURFACE)
                .corner_radius(14)
                .inner_margin(5)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        if w::icon_button(
                            ui,
                            Icon::ToStart,
                            sq(42.0),
                            BtnStyle::Ghost,
                            !recording,
                            "Début (Origine)",
                        )
                        .clicked()
                        {
                            self.seek(0);
                            self.view.scroll = 0.0;
                        }
                        if w::icon_button(
                            ui,
                            Icon::Stop,
                            sq(42.0),
                            BtnStyle::Ghost,
                            !stopped,
                            "Stop (Espace)",
                        )
                        .clicked()
                        {
                            self.stop();
                        }
                        let play_style = if playing {
                            BtnStyle::Active(t::PLAY)
                        } else {
                            BtnStyle::Tinted(t::PLAY)
                        };
                        if w::icon_button(
                            ui,
                            Icon::Play,
                            sq(42.0),
                            play_style,
                            !recording,
                            "Lecture (Espace)",
                        )
                        .clicked()
                        {
                            if playing {
                                self.stop();
                            } else {
                                self.play();
                            }
                        }
                        let blink = (self.now * 1.6).fract() < 0.55;
                        let rec_style = if recording {
                            BtnStyle::Active(if blink {
                                t::REC
                            } else {
                                t::REC.gamma_multiply(0.7)
                            })
                        } else {
                            BtnStyle::Tinted(t::REC)
                        };
                        if w::icon_button(
                            ui,
                            Icon::Record,
                            sq(42.0),
                            rec_style,
                            true,
                            "Enregistrer (R)",
                        )
                        .clicked()
                        {
                            if recording {
                                self.stop();
                            } else {
                                self.record();
                            }
                        }
                    });
                });
            ui.add_space(10.0);

            // Compteur
            let (rect, _) = ui.allocate_exact_size(vec2(210.0, 52.0), Sense::hover());
            let p = ui.painter();
            p.rect_filled(rect, 12.0, t::SURFACE);
            let col = if recording { t::REC } else { t::TEXT };
            w::counter(
                p,
                pos2(rect.center().x, rect.top() + 21.0),
                &fmt_time(self.playhead, self.sr()),
                t::medium(24.0),
                col,
            );
            let sub = match self.transport {
                Transport::Recording { start } => {
                    format!(
                        "● REC  {}",
                        fmt_time_short(self.playhead.saturating_sub(start), self.sr())
                    )
                }
                _ => match self.selection {
                    Some((a, b)) => format!("Sélection {}", fmt_time_short(b - a, self.sr())),
                    None => format!("Durée {}", fmt_time_short(self.project.end(), self.sr())),
                },
            };
            p.text(
                pos2(rect.center().x, rect.bottom() - 11.0),
                Align2::CENTER_CENTER,
                sub,
                t::medium(10.5),
                if recording { t::REC } else { t::TEXT_3 },
            );
            ui.add_space(12.0);

            let follow_col = if self.view.follow {
                Some(t::ACCENT)
            } else {
                None
            };
            if w::pill(ui, Some(Icon::Follow), "Suivre", follow_col, true)
                .on_hover_text("La timeline suit la tête de lecture")
                .clicked()
            {
                self.view.follow = !self.view.follow;
                self.view.nav_until = 0.0;
            }
            if w::pill(ui, Some(Icon::Marker), "Repère", None, true)
                .on_hover_text("Ajouter un repère (M) — possible pendant l'enregistrement")
                .clicked()
            {
                self.add_marker();
            }
            let bridge_col = if self.show_bridge {
                Some(t::ACCENT)
            } else {
                None
            };
            if w::pill(ui, Some(Icon::Meters), "Vumètres", bridge_col, true)
                .on_hover_text("Afficher tous les vumètres d'entrée")
                .clicked()
            {
                self.show_bridge = !self.show_bridge;
            }
            if self.selection.is_some()
                && w::pill(ui, Some(Icon::Close), "Sélection", None, true)
                    .on_hover_text("Effacer la sélection (Échap)")
                    .clicked()
            {
                self.selection = None;
            }
        });

        // Droite
        let right = Rect::from_min_max(pos2(full.right() - 420.0, full.top()), full.max);
        let mut child = ui.new_child(
            UiBuilder::new()
                .max_rect(right)
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
        );
        child.spacing_mut().item_spacing.x = 6.0;
        if w::pill(
            &mut child,
            Some(Icon::Export),
            "Exporter",
            None,
            self.project.has_audio() && stopped,
        )
        .clicked()
        {
            self.open_export();
        }
        let in_ch = self.engine.status.in_channels;
        if w::pill(
            &mut child,
            None,
            &format!("1 piste / entrée ({in_ch})"),
            None,
            stopped && in_ch > 0,
        )
        .on_hover_text("Crée une piste pour chaque entrée non utilisée (idéal pour Dante)")
        .clicked()
        {
            self.add_track_per_input();
        }
        if w::pill(&mut child, Some(Icon::Plus), "Piste", None, stopped).clicked() {
            self.add_track();
        }
    }

    // ---------------------------------------------------------------- timeline

    fn x_of(&self, left: f32, sample: u64) -> f32 {
        left + ((sample as f64 - self.view.scroll) / self.view.spp) as f32
    }

    fn sample_at(&self, left: f32, x: f32) -> u64 {
        (self.view.scroll + (x - left) as f64 * self.view.spp).max(0.0) as u64
    }

    fn follow_playhead(&mut self) {
        if self.is_stopped() || !self.view.follow || self.now < self.view.nav_until {
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

    fn ruler(&mut self, ui: &mut egui::Ui, rect: Rect) {
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 0.0, t::SURFACE);
        let sr = self.sr() as f64;
        let px_per_sec = sr / self.view.spp;
        let steps = [
            0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 900.0,
            1800.0, 3600.0,
        ];
        let step = steps
            .iter()
            .copied()
            .find(|s| s * px_per_sec >= 90.0)
            .unwrap_or(3600.0);
        let t0 = self.view.scroll / sr;
        let t1 = t0 + rect.width() as f64 * self.view.spp / sr;
        let mut tt = (t0 / step).floor() * step;
        while tt <= t1 {
            let x = self.x_of(rect.left(), (tt * sr) as u64);
            p.line_segment(
                [pos2(x, rect.bottom() - 9.0), pos2(x, rect.bottom())],
                Stroke::new(1.0, t::TEXT_3),
            );
            for k in 1..4 {
                let xs = x + (k as f64 * step / 4.0 * px_per_sec) as f32;
                p.line_segment(
                    [pos2(xs, rect.bottom() - 4.0), pos2(xs, rect.bottom())],
                    Stroke::new(1.0, t::SURFACE_4),
                );
            }
            p.text(
                pos2(x + 4.0, rect.top() + 7.0),
                Align2::LEFT_TOP,
                fmt_ruler(tt, step),
                t::regular(10.5),
                t::TEXT_3,
            );
            tt += step;
        }
        // Sélection
        if let Some((a, b)) = self.selection {
            let xa = self.x_of(rect.left(), a).max(rect.left());
            let xb = self.x_of(rect.left(), b).min(rect.right());
            if xb > xa {
                p.rect_filled(
                    Rect::from_min_max(pos2(xa, rect.top()), pos2(xb, rect.bottom())),
                    0.0,
                    t::ACCENT.gamma_multiply(0.35),
                );
            }
        }

        // Repères
        let mut marker_hit = None;
        for m in &self.project.markers {
            let x = self.x_of(rect.left(), m.pos);
            if x < rect.left() - 100.0 || x > rect.right() {
                continue;
            }
            let gal = p.layout_no_wrap(m.name.clone(), t::medium(10.0), Color32::BLACK);
            let r = Rect::from_min_size(pos2(x, rect.top() + 3.0), vec2(gal.size().x + 10.0, 15.0));
            p.rect_filled(
                r,
                CornerRadius {
                    nw: 0,
                    ne: 4,
                    sw: 0,
                    se: 4,
                },
                t::YELLOW,
            );
            p.galley(
                pos2(r.left() + 5.0, r.center().y - gal.size().y / 2.0),
                gal,
                Color32::BLACK,
            );
            p.line_segment(
                [pos2(x, rect.top() + 3.0), pos2(x, rect.bottom())],
                Stroke::new(1.0, t::YELLOW),
            );
            if ui.rect_contains_pointer(r) {
                marker_hit = Some((m.id, r));
            }
        }

        // Tête de lecture
        let x = self.x_of(rect.left(), self.playhead);
        if rect.x_range().contains(x) {
            let tri = vec![
                pos2(x - 6.0, rect.bottom() - 10.0),
                pos2(x + 6.0, rect.bottom() - 10.0),
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
            self.ctx_clip = None;
            mresp.context_menu(|ui| self.marker_menu(ui, id));
            return;
        }
        self.drag_select(&resp, rect.left());
    }

    fn marker_menu(&mut self, ui: &mut egui::Ui, id: u64) {
        let Some(idx) = self.project.markers.iter().position(|m| m.id == id) else {
            return;
        };
        ui.set_width(220.0);
        let r = ui.add(
            egui::TextEdit::singleline(&mut self.project.markers[idx].name).desired_width(200.0),
        );
        if r.changed() {
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
            self.project.markers.remove(idx);
            self.project.dirty = true;
            ui.close();
        }
    }

    /// Clic = déplacer la tête ; glisser = sélection de temps.
    fn drag_select(&mut self, resp: &egui::Response, left: f32) {
        if resp.drag_started()
            && let Some(p) = resp.interact_pointer_pos()
        {
            self.ruler_anchor = Some(self.sample_at(left, p.x));
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
        p.rect_filled(rect, 0.0, t::BG);
        let visible = rect.width() as f64 * self.view.spp;
        let total = (self.project.end().max(self.playhead) as f64 + visible * 0.5)
            .max(self.view.scroll + visible);
        let track = rect.shrink2(vec2(4.0, 4.0));
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
        p.rect_filled(thumb, 4.0, col);
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
        painter.rect_filled(corner, 0.0, t::SURFACE);
        let armed = self.project.tracks.iter().filter(|t| t.armed).count();
        painter.text(
            corner.left_center() + vec2(16.0, 0.0),
            Align2::LEFT_CENTER,
            format!(
                "{} pistes · {} armée{}",
                self.project.tracks.len(),
                armed,
                if armed > 1 { "s" } else { "" }
            ),
            t::medium(11.5),
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
        let in_ch = self.engine.status.in_channels;
        let blink = (self.now * 1.6).fract() < 0.55;

        let mut sa = egui::ScrollArea::vertical()
            .id_salt("tracks_scroll")
            .auto_shrink([false, false]);
        let requested = self.view.want_scroll_y.is_some();
        let prev_y = self.view.scroll_y;
        if let Some(y) = self.view.want_scroll_y.take() {
            sa = sa.vertical_scroll_offset(y);
        }
        let out = sa.show(&mut child, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let ntracks = self.project.tracks.len();
            if ntracks == 0 {
                ui.add_space(40.0);
                ui.vertical_centered(|ui| {
                    ui.label(
                        egui::RichText::new("Aucune piste")
                            .font(t::semibold(15.0))
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
                if !ui.is_rect_visible(row) {
                    continue;
                }
                let color = t::color(idx);
                let header = Rect::from_min_size(row.min, vec2(HEADER_W, track_h));
                let lane = Rect::from_min_max(pos2(row.left() + HEADER_W, row.top()), row.max);
                let tid = self.project.tracks[idx].id;
                let selected = self.selected_tracks.contains(&tid);
                let rec_live = self.live.iter().any(|l| l.track_id == tid);
                let p = ui.painter().clone();
                p.rect_filled(lane, 0.0, if idx % 2 == 0 { t::LANE_A } else { t::LANE_B });
                p.rect_filled(
                    header,
                    0.0,
                    if selected { t::SURFACE_3 } else { t::SURFACE },
                );
                p.rect_filled(
                    Rect::from_min_size(header.min + vec2(0.0, 6.0), vec2(3.0, track_h - 12.0)),
                    CornerRadius::same(2),
                    if rec_live { t::REC } else { color },
                );
                p.line_segment(
                    [
                        pos2(row.left(), row.bottom() - 0.5),
                        pos2(row.right(), row.bottom() - 0.5),
                    ],
                    Stroke::new(1.0, t::BG),
                );

                // Clic sur l'en-tête : sélection (Ctrl / Maj pour plusieurs).
                let hresp =
                    ui.interact(header, ui.make_persistent_id(("hdr", tid)), Sense::click());
                if hresp.clicked() {
                    let m = ui.input(|i| i.modifiers);
                    if m.command || m.shift {
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
                        .max_rect(header.shrink2(vec2(12.0, 9.0)).translate(vec2(4.0, 0.0))),
                );
                hdr.spacing_mut().item_spacing = vec2(6.0, 6.0);
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
                    let r = ui.add_enabled(
                        stopped,
                        egui::TextEdit::singleline(&mut track.name)
                            .desired_width(178.0)
                            .font(t::medium(13.5))
                            .frame(egui::Frame::NONE)
                            .margin(egui::Margin::symmetric(2, 3)),
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
                        vec2(58.0, 20.0),
                        if stopped {
                            Sense::click()
                        } else {
                            Sense::hover()
                        },
                    );
                    let hov = resp.hovered() && stopped;
                    ui.painter()
                        .rect_filled(r, 6.0, if hov { t::SURFACE_4 } else { t::SURFACE_3 });
                    ui.painter().text(
                        r.center(),
                        Align2::CENTER_CENTER,
                        label,
                        t::semibold(10.5),
                        if input.is_some() { t::TEXT } else { t::TEXT_3 },
                    );
                    let resp = resp.on_hover_text("Choisir l'entrée de la carte");
                    egui::Popup::menu(&resp).width(8.0 * 36.0).show(|ui| {
                        new_input = input_picker(ui, input, in_ch, &self.project.tracks, tid);
                    });

                    let m = input
                        .filter(|&i| (i as usize) < audio::MAX_INPUTS && i < in_ch)
                        .map(|i| self.in_meters[i as usize])
                        .unwrap_or_default();
                    let (mr, _) = ui.allocate_exact_size(vec2(160.0, 8.0), Sense::hover());
                    w::meter_h(ui.painter(), mr, &m);
                    let (pr, presp) = ui.allocate_exact_size(vec2(40.0, 18.0), Sense::click());
                    w::peak_readout(ui.painter(), pr, &m);
                    if presp
                        .on_hover_text("Crête max (clic : remise à zéro)")
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

                // ---- piste (timeline)
                let lp = ui.painter_at(lane);
                let inner = lane.shrink2(vec2(0.0, 5.0));
                let track = &self.project.tracks[idx];
                let muted = !self.project.audible().get(idx).copied().unwrap_or(true);
                let ccol = if muted { t::TEXT_3 } else { color };
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
                    draw_clip(
                        &lp,
                        crect,
                        lane,
                        ccol,
                        self.peaks.get(&c.id),
                        c.offset,
                        self.view.spp,
                        Some(&c.file),
                    );
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
                    );
                }

                let resp = ui.interact(
                    lane,
                    ui.make_persistent_id(("lane", track.id)),
                    Sense::click_and_drag(),
                );
                if resp.secondary_clicked() {
                    self.ctx_clip = resp.interact_pointer_pos().and_then(|p| {
                        let s = self.sample_at(lane.left(), p.x);
                        track
                            .clips
                            .iter()
                            .rev()
                            .find(|c| c.start <= s && s < c.end())
                            .map(|c| (track.id, c.id))
                    });
                }
                if resp.double_clicked()
                    && let Some(p) = resp.interact_pointer_pos()
                {
                    let s = self.sample_at(lane.left(), p.x);
                    if let Some(c) = track
                        .clips
                        .iter()
                        .rev()
                        .find(|c| c.start <= s && s < c.end())
                    {
                        self.selection = Some((c.start, c.end()));
                    }
                }
                let tid = track.id;
                self.drag_select(&resp, lane.left());
                resp.context_menu(|ui| match self.ctx_clip {
                    Some((tt, c)) if tt == tid => {
                        if ui.button("Sélectionner la prise").clicked() {
                            if let Some(cl) =
                                self.project.tracks[idx].clips.iter().find(|x| x.id == c)
                            {
                                self.selection = Some((cl.start, cl.end()));
                            }
                            ui.close();
                        }
                        if ui
                            .add_enabled(stopped, egui::Button::new("Supprimer la prise"))
                            .clicked()
                        {
                            clip_to_delete = Some((tt, c));
                            ui.close();
                        }
                    }
                    _ => {
                        ui.label(egui::RichText::new("Aucune prise ici").color(t::TEXT_3));
                    }
                });
            }
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                ui.add_space(14.0);
                if w::pill(ui, Some(Icon::Plus), "Ajouter une piste", None, stopped).clicked() {
                    self.add_track();
                }
            });
            ui.add_space(40.0);
        });
        self.view.scroll_y = out.state.clamped_offset().y;
        self.view.viewport_h = out.inner_rect.height();
        if !requested && (out.state.clamped_offset().y - prev_y).abs() > 0.5 {
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
            }
        }
        for m in &self.project.markers {
            let x = self.x_of(tl_left, m.pos);
            if over.x_range().contains(x) {
                op.line_segment(
                    [pos2(x, over.top()), pos2(x, over.bottom())],
                    Stroke::new(1.0, t::YELLOW.gamma_multiply(0.45)),
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
            op.line_segment(
                [pos2(x, over.top()), pos2(x, over.bottom())],
                Stroke::new(1.5, col),
            );
        }
        self.scrollbar(ui, sb_rect);
        // Bloc gauche sous les en-têtes (au niveau de la barre de défilement).
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
            let id = self.project.tracks[i].id;
            self.selected_tracks.remove(&id);
            self.project.tracks.remove(i);
            self.project.dirty = true;
        }
        if let Some((tt, c)) = clip_to_delete
            && let Some(track) = self.project.track_mut(tt)
        {
            track.clips.retain(|x| x.id != c);
            self.project.dirty = true;
        }
    }

    // ---------------------------------------------------------------- bandeau de vumètres

    fn meter_bridge(&mut self, ui: &mut egui::Ui) {
        let full = ui.max_rect();
        let in_ch = self.engine.status.in_channels;
        let p = ui.painter().clone();
        p.line_segment(
            [full.left_top(), full.right_top()],
            Stroke::new(1.0, t::BORDER),
        );
        let top = full.top() + 34.0;
        let bottom = full.bottom() - 40.0;
        p.text(
            pos2(full.left() + 16.0, full.top() + 16.0),
            Align2::LEFT_CENTER,
            "ENTRÉES",
            t::semibold(10.5),
            t::TEXT_3,
        );
        p.text(
            pos2(full.left() + 80.0, full.top() + 16.0),
            Align2::LEFT_CENTER,
            "clic sur une valeur crête : remise à zéro",
            t::regular(10.5),
            t::TEXT_3,
        );
        // Échelle fixe à gauche.
        w::scale_v(
            &p,
            Rect::from_min_max(
                pos2(full.left() + 8.0, top),
                pos2(full.left() + 26.0, bottom),
            ),
            true,
        );
        let area = Rect::from_min_max(pos2(full.left() + 32.0, full.top() + 26.0), full.max);
        let mut child = ui.new_child(UiBuilder::new().max_rect(area));
        egui::ScrollArea::horizontal()
            .id_salt("bridge")
            .auto_shrink([false, false])
            .show(&mut child, |ui| {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    let col_w = 46.0;
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
                            p.rect_filled(
                                r.shrink(1.0),
                                8.0,
                                if sel { t::SURFACE_3 } else { t::SURFACE_2 },
                            );
                        }
                        let m = input
                            .filter(|&i| (i as usize) < audio::MAX_INPUTS && i < in_ch)
                            .map(|i| self.in_meters[i as usize])
                            .unwrap_or_default();
                        let pr = Rect::from_center_size(
                            pos2(r.center().x, r.top() + 8.0),
                            vec2(34.0, 16.0),
                        );
                        w::peak_readout(p, pr, &m);
                        let mr = Rect::from_min_max(
                            pos2(r.center().x - 5.0, top),
                            pos2(r.center().x + 5.0, bottom),
                        );
                        w::meter_v(p, mr, &m);
                        let num_col = if armed { t::REC } else { t::color(idx) };
                        p.text(
                            pos2(r.center().x, bottom + 12.0),
                            Align2::CENTER_CENTER,
                            format!("{}", idx + 1),
                            t::semibold(11.0),
                            num_col,
                        );
                        p.text(
                            pos2(r.center().x, bottom + 27.0),
                            Align2::CENTER_CENTER,
                            short(&name, 6),
                            t::regular(9.5),
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
    ui.label(
        egui::RichText::new("ENTRÉE")
            .font(t::semibold(10.5))
            .color(t::TEXT_3),
    );
    if in_ch == 0 {
        ui.label(egui::RichText::new("Aucune carte son").color(t::TEXT_3));
    }
    let used: Vec<u16> = tracks
        .iter()
        .filter(|t| t.id != me)
        .filter_map(|t| t.input)
        .collect();
    egui::Grid::new("inpick")
        .spacing([4.0, 4.0])
        .show(ui, |ui| {
            for i in 0..in_ch {
                let sel = current == Some(i);
                let is_used = used.contains(&i);
                let (r, resp) = ui.allocate_exact_size(vec2(32.0, 26.0), Sense::click());
                let bg = if sel {
                    t::ACCENT
                } else if resp.hovered() {
                    t::SURFACE_4
                } else {
                    t::SURFACE_3
                };
                ui.painter().rect_filled(r, 6.0, bg);
                let fg = if sel {
                    Color32::WHITE
                } else if is_used {
                    t::TEXT_3
                } else {
                    t::TEXT
                };
                ui.painter().text(
                    r.center(),
                    Align2::CENTER_CENTER,
                    format!("{}", i + 1),
                    t::medium(11.5),
                    fg,
                );
                if is_used && !sel {
                    ui.painter().circle_filled(
                        pos2(r.right() - 5.0, r.top() + 5.0),
                        2.0,
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
) {
    let cr = CornerRadius::same(6);
    painter.rect_filled(rect, cr, color.gamma_multiply(0.16));
    painter.rect_stroke(
        rect,
        cr,
        Stroke::new(1.0, color.gamma_multiply(0.55)),
        StrokeKind::Inside,
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
                Rect::from_min_max(pos2(x0, rect.top() + 14.0), pos2(x1, rect.bottom() - 3.0));
            let mesh = w::wave_mesh(wave, color.gamma_multiply(0.9), |i| {
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
    if let Some(label) = label
        && rect.width() > 50.0
    {
        let pos = Pos2::new(rect.left().max(visible.left()) + 7.0, rect.top() + 3.0);
        painter.text(pos, Align2::LEFT_TOP, label, t::medium(10.0), color);
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
