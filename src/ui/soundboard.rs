//! Onglet Soundboard : grille de pads, pages, contrôleur MIDI, éditeur de son.

use super::multitrack::master_panel;
use super::theme as t;
use super::widgets::{self as w, BtnStyle, Icon, sq};
use crate::app::{App, short};
use crate::midi::{ALL_PORTS, list_ports};
use crate::sampler::{MAX_PAGES, PadCmd, PadMode, SAMPLE_PEAK, SampleData};
use crate::soundboard::{Board, LoadState, Pad, Page, is_audio_file};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, Key, Rect, Sense, Stroke, StrokeKind, UiBuilder, pos2,
    vec2,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Handle {
    Start,
    End,
    FadeIn,
    FadeOut,
}

fn fmt_secs(s: f64) -> String {
    let s = s.max(0.0);
    let m = (s / 60.0).floor() as u64;
    format!("{}:{:04.1}", m, s - m as f64 * 60.0)
}

impl App {
    // ---------------------------------------------------------------- logique

    pub fn press_pad(&mut self, slot: u16) {
        let Some(pad) = self.board.pad(slot) else {
            return;
        };
        let Some(sample) = self.cache.get(&pad.path).cloned() else {
            return;
        };
        let params = pad.params(&sample);
        self.engine.pad(PadCmd::Press {
            slot,
            sample,
            params,
        });
        self.sb.flash.insert(slot, self.now);
    }

    pub fn release_pad(&mut self, slot: u16) {
        self.engine.pad(PadCmd::Release { slot });
    }

    pub fn stop_all_pads(&mut self) {
        let fade = (self.board.panic_fade * self.sr() as f64) as usize;
        self.engine.pad(PadCmd::StopAll { fade });
    }

    fn pad_playing(&self, slot: u16) -> Option<u64> {
        let v = self
            .engine
            .shared
            .pad_pos
            .get(slot as usize)?
            .load(Ordering::Relaxed);
        (v > 0).then(|| v - 1)
    }

    fn assign_files(&mut self, start_slot: u16, files: Vec<PathBuf>) {
        let mut slot = start_slot as usize;
        let page = start_slot as usize / crate::sampler::PADS_PER_PAGE;
        let page_end = (page + 1) * crate::sampler::PADS_PER_PAGE;
        let mut first = true;
        for f in files.into_iter().filter(|f| is_audio_file(f)) {
            // Le premier fichier remplace le pad visé, les suivants vont dans les pads vides.
            if !first {
                while slot < page_end && self.board.pad(slot as u16).is_some() {
                    slot += 1;
                }
            }
            if slot >= page_end {
                break;
            }
            let color = (slot % t::PALETTE.len()) as u8;
            let old = self.board.pad(slot as u16).cloned();
            let mut pad = Pad::new(f.clone(), color);
            if let Some(o) = old {
                // On garde les réglages de déclenchement du pad remplacé.
                pad.color = o.color;
                pad.key = o.key;
                pad.midi = o.midi;
                pad.mode = o.mode;
            }
            self.cache.request(&f, None);
            self.board.set(slot as u16, Some(pad));
            self.sb.selected = Some(slot as u16);
            slot += 1;
            first = false;
        }
        self.board_modified();
    }

    /// Traitement indépendant de l'onglet affiché : MIDI, table de déclenchement.
    pub fn soundboard_background(&mut self, ctx: &egui::Context) {
        while let Ok(e) = self.midi.events.try_recv() {
            self.sb.midi_seen = self.now;
            self.sb.midi_last = Some(e.key.label());
            if !e.press {
                continue;
            }
            if let Some(slot) = self.sb.learn_midi.take() {
                for page in &mut self.board.pages {
                    for p in page.pads.iter_mut().flatten() {
                        if p.midi == Some(e.key) {
                            p.midi = None;
                        }
                    }
                }
                if let Some(p) = self.board.pad_mut(slot) {
                    p.midi = Some(e.key);
                }
                self.board_modified();
                self.notify(format!("MIDI {} assigné", e.key.label()), false);
            } else if let Some(slot) = self
                .board
                .all_pads()
                .find(|(_, p)| p.midi == Some(e.key))
                .map(|(s, _)| s)
            {
                self.sb.flash.insert(slot, self.now);
            }
        }
        if self.sb.midi_dirty {
            self.sb.midi_dirty = false;
            let mut map = HashMap::new();
            for (slot, pad) in self.board.all_pads() {
                if let (Some(k), Some(s)) = (pad.midi, self.cache.get(&pad.path)) {
                    map.insert(k, (slot, s.clone(), pad.params(s)));
                }
            }
            self.midi.shared.lock().map = map;
        }
        let _ = ctx;
    }

    fn soundboard_keys(&mut self, ctx: &egui::Context) {
        if self.dialog_open() || ctx.egui_wants_keyboard_input() {
            return;
        }
        let events: Vec<(Key, bool, bool)> = ctx.input(|i| {
            i.events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Key {
                        key,
                        pressed,
                        repeat,
                        modifiers,
                        ..
                    } if !modifiers.command && !modifiers.alt => Some((*key, *pressed, *repeat)),
                    _ => None,
                })
                .collect()
        });
        for (key, pressed, repeat) in events {
            if repeat {
                continue;
            }
            if let Some(slot) = self.sb.learn_key {
                if pressed {
                    self.sb.learn_key = None;
                    if key != Key::Escape {
                        let name = key.name().to_string();
                        for page in &mut self.board.pages {
                            for p in page.pads.iter_mut().flatten() {
                                if p.key.as_deref() == Some(name.as_str()) {
                                    p.key = None;
                                }
                            }
                        }
                        if let Some(p) = self.board.pad_mut(slot) {
                            p.key = Some(name);
                        }
                        self.board_modified();
                    }
                }
                continue;
            }
            if key == Key::Escape && pressed {
                self.stop_all_pads();
                continue;
            }
            if pressed {
                let name = key.name();
                let found = self
                    .board
                    .all_pads()
                    .find(|(_, p)| p.key.as_deref() == Some(name))
                    .map(|(s, _)| s);
                if let Some(slot) = found {
                    self.sb.held_keys.insert(key, slot);
                    self.press_pad(slot);
                }
            } else if let Some(slot) = self.sb.held_keys.remove(&key) {
                self.release_pad(slot);
            }
        }
    }

    // ---------------------------------------------------------------- interface

    pub fn soundboard_ui(&mut self, ui: &mut egui::Ui) {
        self.soundboard_keys(ui.ctx());
        if self.sb.page >= self.board.pages.len() {
            self.sb.page = 0;
        }
        egui::Panel::top("sb_head")
            .exact_size(64.0)
            .frame(
                egui::Frame::new()
                    .fill(t::BG)
                    .inner_margin(egui::Margin::symmetric(16, 0)),
            )
            .show(ui, |ui| self.sb_header(ui));
        egui::Panel::right("sb_master")
            .exact_size(104.0)
            .frame(egui::Frame::new().fill(t::SURFACE))
            .show(ui, |ui| {
                let mut m = self.master_pad;
                master_panel(ui, "PADS", &mut m);
                self.master_pad = m;
            });
        if self.sb.selected.and_then(|s| self.board.pad(s)).is_some() {
            egui::Panel::bottom("sb_editor")
                .resizable(true)
                .default_size(340.0)
                .min_size(260.0)
                .max_size(520.0)
                .frame(
                    egui::Frame::new()
                        .fill(t::SURFACE)
                        .inner_margin(egui::Margin::same(16)),
                )
                .show(ui, |ui| self.pad_editor(ui));
        }
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(t::BG)
                    .inner_margin(egui::Margin::symmetric(16, 8)),
            )
            .show(ui, |ui| self.pad_grid(ui));
    }

    fn sb_header(&mut self, ui: &mut egui::Ui) {
        let full = ui.max_rect();
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let mut remove_page = None;
            for i in 0..self.board.pages.len() {
                let active = i == self.sb.page;
                let name = self.board.pages[i].name.clone();
                let r = w::pill(
                    ui,
                    None,
                    &name,
                    if active { Some(t::SURFACE_4) } else { None },
                    true,
                );
                if r.clicked() {
                    self.sb.page = i;
                }
                r.context_menu(|ui| {
                    ui.set_width(200.0);
                    if ui
                        .add(
                            egui::TextEdit::singleline(&mut self.board.pages[i].name)
                                .desired_width(180.0),
                        )
                        .changed()
                    {
                        self.board_changed = Some(self.now);
                    }
                    if self.board.pages.len() > 1 && ui.button("Supprimer la page").clicked() {
                        remove_page = Some(i);
                        ui.close();
                    }
                });
            }
            if let Some(i) = remove_page {
                self.board.pages.remove(i);
                self.sb.page = self.sb.page.min(self.board.pages.len() - 1);
                self.sb.selected = None;
                self.board_modified();
            }
            if self.board.pages.len() < MAX_PAGES
                && w::icon_button(
                    ui,
                    Icon::Plus,
                    sq(30.0),
                    BtnStyle::Ghost,
                    true,
                    "Nouvelle page",
                )
                .clicked()
            {
                let n = self.board.pages.len() + 1;
                self.board.pages.push(Page::new(format!("Page {n}")));
                self.sb.page = self.board.pages.len() - 1;
                self.board_modified();
            }
        });

        let right = Rect::from_min_max(pos2(full.right() - 760.0, full.top()), full.max);
        let mut child = ui.new_child(
            UiBuilder::new()
                .max_rect(right)
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
        );
        let ui = &mut child;
        ui.spacing_mut().item_spacing.x = 8.0;
        if w::pill(
            ui,
            Some(Icon::Stop),
            "Tout arrêter",
            Some(t::REC.gamma_multiply(0.85)),
            true,
        )
        .on_hover_text(format!(
            "Fondu de sortie {:.1} s sur tous les pads (Échap)",
            self.board.panic_fade
        ))
        .clicked()
        {
            self.stop_all_pads();
        }

        // Volume général des pads
        let mut db = self.board.master_db;
        let r = ui.add(
            egui::Slider::new(&mut db, -40.0..=6.0)
                .show_value(false)
                .trailing_fill(true),
        );
        ui.label(
            egui::RichText::new(format!("{} dB", w::fmt_db(db)))
                .font(t::medium(11.5))
                .color(t::TEXT_2),
        );
        if r.changed() {
            self.board.master_db = db;
            self.engine.pad(PadCmd::SetMaster {
                gain: crate::soundboard::db_to_gain(db),
            });
            self.board_changed = Some(self.now);
        }
        if r.double_clicked() {
            self.board.master_db = 0.0;
            self.engine.pad(PadCmd::SetMaster { gain: 1.0 });
        }
        let (ir, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
        w::paint_icon(ui.painter(), Icon::Speaker, ir.center(), 6.5, t::TEXT_2);

        // Sorties
        let out_ch = self.engine.status.out_channels.max(2);
        let l = self.board.out_left;
        egui::ComboBox::from_id_salt("sb_out")
            .width(118.0)
            .height(320.0)
            .selected_text(format!("Sorties {} – {}", l + 1, l + 2))
            .show_ui(ui, |ui| {
                let mut i = 0;
                while i < out_ch {
                    if ui
                        .selectable_label(l == i, format!("Sorties {} – {}", i + 1, i + 2))
                        .clicked()
                    {
                        self.board.out_left = i;
                        self.engine.pad(PadCmd::SetOutput { left: i as usize });
                        self.board_changed = Some(self.now);
                    }
                    i += 2;
                }
            });

        // Contrôleur MIDI
        let active = self.now - self.sb.midi_seen < 0.15;
        let cur = self
            .board
            .midi_port
            .clone()
            .unwrap_or_else(|| "Aucun contrôleur".into());
        let mut choose: Option<Option<String>> = None;
        let combo = egui::ComboBox::from_id_salt("sb_midi")
            .width(170.0)
            .selected_text(short(&cur, 22))
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(self.board.midi_port.is_none(), "Aucun contrôleur")
                    .clicked()
                {
                    choose = Some(None);
                }
                if ui
                    .selectable_label(
                        self.board.midi_port.as_deref() == Some(ALL_PORTS),
                        ALL_PORTS,
                    )
                    .clicked()
                {
                    choose = Some(Some(ALL_PORTS.into()));
                }
                for p in self.midi_ports.clone() {
                    if ui
                        .selectable_label(self.board.midi_port.as_deref() == Some(&p), &p)
                        .clicked()
                    {
                        choose = Some(Some(p));
                    }
                }
            });
        if combo.response.clicked() {
            self.midi_ports = list_ports();
        }
        let (dr, _) = ui.allocate_exact_size(vec2(12.0, 12.0), Sense::hover());
        let dot = if active {
            t::PLAY
        } else if self.midi.connected.is_empty() {
            t::SURFACE_4
        } else {
            t::PLAY.gamma_multiply(0.35)
        };
        ui.painter().circle_filled(dr.center(), 4.0, dot);
        let tip = match (&self.midi.error, &self.sb.midi_last) {
            (Some(e), _) => format!("Erreur MIDI : {e}"),
            (None, Some(l)) => format!("Dernier message : {l}"),
            _ => "Contrôleur MIDI USB (Launchpad, APC, nanoPAD…)".into(),
        };
        combo.response.on_hover_text(tip);
        if let Some(c) = choose {
            self.board.midi_port = c.clone();
            self.midi.connect(c.as_deref());
            self.board_changed = Some(self.now);
            if let Some(e) = self.midi.error.clone() {
                self.notify(format!("MIDI : {e}"), true);
            } else if !self.midi.connected.is_empty() {
                self.notify(
                    format!("MIDI connecté : {}", self.midi.connected.join(", ")),
                    false,
                );
            }
        }

        // Taille de la grille
        let (c, r) = (self.board.cols, self.board.rows);
        egui::ComboBox::from_id_salt("sb_grid")
            .width(76.0)
            .selected_text(format!("{c} × {r}"))
            .show_ui(ui, |ui| {
                for (cc, rr) in [(4, 2), (4, 4), (5, 3), (6, 4), (8, 4), (8, 6), (8, 8)] {
                    if ui
                        .selectable_label(c == cc && r == rr, format!("{cc} × {rr}"))
                        .clicked()
                    {
                        self.board.cols = cc;
                        self.board.rows = rr;
                        self.board_changed = Some(self.now);
                    }
                }
            });
    }

    fn pad_grid(&mut self, ui: &mut egui::Ui) {
        let area = ui.available_rect_before_wrap();
        let (cols, rows) = (self.board.cols, self.board.rows);
        let gap = 10.0;
        let pw = ((area.width() - gap * (cols as f32 - 1.0)) / cols as f32).max(40.0);
        let ph =
            ((area.height() - gap * (rows as f32 - 1.0)) / rows as f32).clamp(40.0, pw.max(90.0));
        let page = self.sb.page;
        let pointer = ui.input(|i| i.pointer.hover_pos().or(i.pointer.latest_pos()));
        let dragging_files = ui.input(|i| !i.raw.hovered_files.is_empty());
        let dropped: Vec<PathBuf> = ui.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .filter(|p| !p.as_os_str().is_empty())
                .collect()
        });
        // Position de l'appui lue dans les événements : fiable même si appui et relâchement
        // arrivent dans la même image (tap sur trackpad / écran tactile).
        let (press_pos, primary_released) = ui.input(|i| {
            let pos = i.events.iter().find_map(|e| match e {
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    ..
                } => Some(*pos),
                _ => None,
            });
            let rel = i.events.iter().any(|e| {
                matches!(
                    e,
                    egui::Event::PointerButton {
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        ..
                    }
                )
            });
            (pos, rel)
        });
        let mut drop_slot = None;
        let mut empty_clicked = None;
        let blocked = self.dialog_open() || ui.ctx().any_popup_open();

        for idx in 0..(cols * rows) {
            let (c, r) = (idx % cols, idx / cols);
            let rect = Rect::from_min_size(
                pos2(
                    area.left() + c as f32 * (pw + gap),
                    area.top() + r as f32 * (ph + gap),
                ),
                vec2(pw, ph),
            );
            let slot = Board::slot(page, idx);
            let resp = ui.interact(
                rect,
                ui.make_persistent_id(("pad", slot)),
                Sense::click_and_drag(),
            );
            let hovered_drop = dragging_files && pointer.is_some_and(|p| rect.contains(p));
            if !dropped.is_empty() && pointer.is_some_and(|p| rect.contains(p)) {
                drop_slot = Some(slot);
            }
            let p = ui.painter();
            let Some(pad) = self.board.pad(slot).cloned() else {
                let col = if hovered_drop {
                    t::ACCENT
                } else if resp.hovered() {
                    t::TEXT_3
                } else {
                    t::SURFACE_4
                };
                p.rect_filled(
                    rect,
                    CornerRadius::same(t::R_LARGE),
                    if hovered_drop {
                        t::ACCENT.gamma_multiply(0.12)
                    } else {
                        t::SURFACE
                    },
                );
                w::dashed_rect(p, rect.shrink(6.0), col);
                w::paint_icon(p, Icon::Plus, rect.center() - vec2(0.0, 8.0), 7.0, col);
                p.text(
                    rect.center() + vec2(0.0, 14.0),
                    Align2::CENTER_CENTER,
                    "Glisser un son",
                    t::regular(11.0),
                    col,
                );
                if resp.clicked() {
                    empty_clicked = Some(slot);
                }
                continue;
            };

            // Déclenchement à l'appui (pas au relâchement) pour la réactivité.
            if !blocked && press_pos.is_some_and(|p| rect.contains(p)) {
                self.sb.mouse_down = Some(slot);
                self.sb.selected = Some(slot);
                self.press_pad(slot);
            }
            resp.context_menu(|ui| {
                if ui.button("Remplacer le son…").clicked() {
                    if let Some(f) = pick_audio_files(false) {
                        self.assign_files(slot, f);
                    }
                    ui.close();
                }
                if ui.button("Arrêter").clicked() {
                    self.engine.pad(PadCmd::Stop { slot });
                    ui.close();
                }
                if ui.button("Supprimer le pad").clicked() {
                    self.engine.pad(PadCmd::Stop { slot });
                    self.board.set(slot, None);
                    if self.sb.selected == Some(slot) {
                        self.sb.selected = None;
                    }
                    self.board_modified();
                    ui.close();
                }
            });

            let color = t::color(pad.color as usize);
            let playing = self.pad_playing(slot);
            let flash = self
                .sb
                .flash
                .get(&slot)
                .is_some_and(|&f| self.now - f < 0.12);
            let selected = self.sb.selected == Some(slot);
            let cr = CornerRadius::same(t::R_LARGE);
            let sample = self.cache.get(&pad.path).cloned();

            if playing.is_some() {
                p.rect_filled(
                    rect.expand(3.0),
                    CornerRadius::same(t::R_LARGE + 3),
                    color.gamma_multiply(0.18),
                );
            }
            let base = if playing.is_some() || flash {
                blend(t::SURFACE_2, color, 0.30)
            } else if resp.hovered() {
                blend(t::SURFACE_2, color, 0.12)
            } else {
                blend(t::SURFACE_2, color, 0.07)
            };
            p.rect_filled(rect, cr, base);
            let border = if hovered_drop {
                Stroke::new(2.0, t::ACCENT)
            } else if selected {
                Stroke::new(1.5, t::TEXT.gamma_multiply(0.85))
            } else if playing.is_some() {
                Stroke::new(1.5, color)
            } else {
                Stroke::new(1.0, color.gamma_multiply(0.28))
            };
            p.rect_stroke(rect, cr, border, StrokeKind::Inside);

            // Pastille de couleur + numéro
            p.circle_filled(pos2(rect.left() + 16.0, rect.top() + 16.0), 4.5, color);
            p.text(
                pos2(rect.right() - 12.0, rect.top() + 16.0),
                Align2::RIGHT_CENTER,
                format!("{}", idx + 1),
                t::medium(10.0),
                t::TEXT_3,
            );
            // Nom (2 lignes max)
            let name_rect = Rect::from_min_max(
                pos2(rect.left() + 14.0, rect.top() + 28.0),
                pos2(rect.right() - 12.0, rect.bottom() - 26.0),
            );
            let mut job = egui::text::LayoutJob::simple(
                pad.name.clone(),
                t::semibold(14.0),
                t::TEXT,
                name_rect.width(),
            );
            job.wrap.max_rows = if ph > 100.0 { 3 } else { 2 };
            let gal = p.layout_job(job);
            p.galley(name_rect.min, gal, t::TEXT);

            // Bas du pad : durée / état, badges.
            let by = rect.bottom() - 15.0;
            let (status, scol) = match self.cache.map.get(&pad.path) {
                Some(LoadState::Ready(s)) => {
                    let pr = pad.params(s);
                    let len = (pr.end - pr.start) as f64 / s.sample_rate as f64;
                    match playing {
                        Some(pos) => {
                            let rem = (pr.end as f64 - pos as f64).max(0.0) / s.sample_rate as f64;
                            (format!("−{}", fmt_secs(rem)), t::TEXT)
                        }
                        None => (fmt_secs(len), t::TEXT_2),
                    }
                }
                Some(LoadState::Failed(_)) => ("Fichier illisible".into(), t::REC),
                _ => ("Chargement…".into(), t::TEXT_3),
            };
            p.text(
                pos2(rect.left() + 14.0, by),
                Align2::LEFT_CENTER,
                status,
                t::medium(11.0),
                scol,
            );
            let mut bx = rect.right() - 10.0;
            let mut badge = |txt: String, col: Color32| {
                let g = p.layout_no_wrap(txt, t::semibold(9.5), col);
                let r = Rect::from_min_size(
                    pos2(bx - g.size().x - 10.0, by - 8.0),
                    vec2(g.size().x + 10.0, 16.0),
                );
                p.rect_filled(r, 5.0, Color32::from_black_alpha(70));
                p.galley(
                    pos2(r.left() + 5.0, r.center().y - g.size().y / 2.0),
                    g,
                    col,
                );
                bx = r.left() - 4.0;
            };
            if let Some(m) = pad.midi {
                badge(m.label(), t::TEXT_2);
            }
            if let Some(k) = &pad.key {
                badge(k.clone(), t::TEXT_2);
            }
            if pad.looped {
                badge("BOUCLE".into(), color);
            }
            match pad.mode {
                PadMode::Toggle => badge("PLAY/STOP".into(), t::TEXT_3),
                PadMode::Gate => badge("MAINTIEN".into(), t::TEXT_3),
                PadMode::OneShot => {}
            }
            // Progression
            if let (Some(pos), Some(s)) = (playing, sample) {
                let pr = pad.params(&s);
                let f = ((pos as f64 - pr.start as f64) / (pr.end - pr.start).max(1) as f64)
                    .clamp(0.0, 1.0) as f32;
                let bar = Rect::from_min_max(
                    pos2(rect.left() + 10.0, rect.bottom() - 5.0),
                    pos2(rect.right() - 10.0, rect.bottom() - 3.0),
                );
                p.rect_filled(bar, 1.0, Color32::from_black_alpha(80));
                p.rect_filled(
                    Rect::from_min_size(bar.min, vec2(bar.width() * f, bar.height())),
                    1.0,
                    color,
                );
            }
            if self.sb.learn_midi == Some(slot) || self.sb.learn_key == Some(slot) {
                let blink = (self.now * 3.0).fract() < 0.5;
                if blink {
                    p.rect_stroke(rect, cr, Stroke::new(2.0, t::ACCENT), StrokeKind::Inside);
                }
            }
        }

        if let (Some(slot), true) = (self.sb.mouse_down, primary_released) {
            self.release_pad(slot);
            self.sb.mouse_down = None;
        }
        if !dropped.is_empty() {
            let target = drop_slot.or_else(|| {
                (0..cols * rows)
                    .map(|i| Board::slot(page, i))
                    .find(|s| self.board.pad(*s).is_none())
            });
            if let Some(s) = target {
                self.assign_files(s, dropped);
            }
        }
        if let Some(s) = empty_clicked
            && let Some(f) = pick_audio_files(true)
        {
            self.assign_files(s, f);
        }
    }

    // ---------------------------------------------------------------- éditeur

    fn pad_editor(&mut self, ui: &mut egui::Ui) {
        let Some(slot) = self.sb.selected else {
            return;
        };
        let Some(mut pad) = self.board.pad(slot).cloned() else {
            return;
        };
        let sample = self.cache.get(&pad.path).cloned();
        let full = ui.max_rect();
        let col_w = 290.0;
        let params_w = col_w * 2.0 + 24.0;
        let wave_rect = Rect::from_min_max(
            full.min,
            pos2(full.right() - params_w - 20.0, full.bottom()),
        );
        let col_a = Rect::from_min_max(
            pos2(full.right() - params_w, full.top()),
            pos2(full.right() - col_w - 24.0, full.bottom()),
        );
        let col_b = Rect::from_min_max(pos2(full.right() - col_w, full.top()), full.max);
        let before = format!("{pad:?}");

        // ---- forme d'onde
        let mut wui = ui.new_child(UiBuilder::new().max_rect(wave_rect));
        self.wave_editor(&mut wui, slot, &mut pad, sample.as_ref());

        let mut delete = false;
        let mut replace = false;
        let dur = sample
            .as_ref()
            .map_or(3600.0, |s| s.frames as f64 / s.sample_rate as f64);

        // ---- colonne A : identité et lecture
        let mut ua = ui.new_child(UiBuilder::new().max_rect(col_a));
        ua.spacing_mut().item_spacing.y = 9.0;
        ua.add(
            egui::TextEdit::singleline(&mut pad.name)
                .font(t::semibold(15.0))
                .desired_width(col_w),
        );
        ua.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 5.0;
            for (i, c) in t::PALETTE.iter().enumerate() {
                let (r, resp) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::click());
                ui.painter().circle_filled(r.center(), 8.0, *c);
                if pad.color as usize == i {
                    ui.painter()
                        .circle_stroke(r.center(), 10.5, Stroke::new(1.5, t::TEXT));
                }
                if resp.clicked() {
                    pad.color = i as u8;
                }
            }
        });
        ua.horizontal(|ui| {
            for m in PadMode::ALL {
                ui.selectable_value(&mut pad.mode, m, m.label());
            }
        });
        ua.checkbox(&mut pad.looped, "Lecture en boucle");
        ua.horizontal(|ui| {
            ui.label(egui::RichText::new("Gain").color(t::TEXT_2));
            ui.add(
                egui::Slider::new(&mut pad.gain_db, -40.0..=12.0)
                    .step_by(0.5)
                    .suffix(" dB")
                    .trailing_fill(true),
            );
        });
        ua.horizontal(|ui| {
            if w::pill(ui, Some(Icon::Play), "Écouter", None, sample.is_some()).clicked() {
                self.press_pad(slot);
            }
            if w::pill(ui, Some(Icon::Stop), "Stop", None, true).clicked() {
                self.engine.pad(PadCmd::Stop { slot });
            }
        });
        ua.label(
            egui::RichText::new(short(&pad.path.display().to_string(), 46))
                .font(t::regular(10.5))
                .color(t::TEXT_3),
        )
        .on_hover_text(pad.path.display().to_string());
        if let Some(LoadState::Failed(e)) = self.cache.map.get(&pad.path) {
            ua.label(
                egui::RichText::new(e.clone())
                    .font(t::regular(11.0))
                    .color(t::REC),
            );
            if ua.button("Réessayer").clicked() {
                let p = pad.path.clone();
                self.cache.reload(&p);
            }
        }

        // ---- colonne B : calage et déclenchement
        let mut ub = ui.new_child(UiBuilder::new().max_rect(col_b));
        ub.spacing_mut().item_spacing.y = 9.0;
        ub.label(
            egui::RichText::new("CALAGE")
                .font(t::semibold(10.5))
                .color(t::TEXT_3),
        );
        egui::Grid::new("pad_times")
            .num_columns(4)
            .spacing([8.0, 8.0])
            .show(&mut ub, |ui| {
                let mut end = pad.end.unwrap_or(dur);
                ui.label(egui::RichText::new("Début").color(t::TEXT_2));
                ui.add(
                    egui::DragValue::new(&mut pad.start)
                        .speed(0.005)
                        .range(0.0..=dur)
                        .fixed_decimals(3)
                        .suffix(" s"),
                );
                ui.label(egui::RichText::new("Fin").color(t::TEXT_2));
                if ui
                    .add(
                        egui::DragValue::new(&mut end)
                            .speed(0.005)
                            .range(0.0..=dur)
                            .fixed_decimals(3)
                            .suffix(" s"),
                    )
                    .changed()
                {
                    pad.end = Some(end);
                }
                ui.end_row();
                ui.label(egui::RichText::new("Fondu in").color(t::TEXT_2));
                ui.add(
                    egui::DragValue::new(&mut pad.fade_in)
                        .speed(0.005)
                        .range(0.0..=dur)
                        .fixed_decimals(2)
                        .suffix(" s"),
                );
                ui.label(egui::RichText::new("Fondu out").color(t::TEXT_2));
                ui.add(
                    egui::DragValue::new(&mut pad.fade_out)
                        .speed(0.005)
                        .range(0.0..=dur)
                        .fixed_decimals(2)
                        .suffix(" s"),
                );
                ui.end_row();
            });
        ub.label(
            egui::RichText::new("DÉCLENCHEMENT")
                .font(t::semibold(10.5))
                .color(t::TEXT_3),
        );
        ub.horizontal(|ui| {
            ui.label(egui::RichText::new("Clavier").color(t::TEXT_2));
            let learning = self.sb.learn_key == Some(slot);
            let txt = if learning {
                "Appuyez sur une touche…".to_string()
            } else {
                pad.key
                    .clone()
                    .unwrap_or_else(|| "Assigner une touche".into())
            };
            if ui.add(egui::Button::new(txt).selected(learning)).clicked() {
                self.sb.learn_key = if learning { None } else { Some(slot) };
                self.sb.learn_midi = None;
            }
            if pad.key.is_some()
                && w::icon_button(ui, Icon::Close, sq(24.0), BtnStyle::Ghost, true, "Retirer")
                    .clicked()
            {
                pad.key = None;
            }
        });
        ub.horizontal(|ui| {
            ui.label(egui::RichText::new("MIDI    ").color(t::TEXT_2));
            let learning = self.sb.learn_midi == Some(slot);
            let txt = if learning {
                "Touchez un pad du contrôleur…".to_string()
            } else {
                pad.midi
                    .map_or("Apprendre (MIDI learn)".into(), |m| m.label())
            };
            if ui.add(egui::Button::new(txt).selected(learning)).clicked() {
                self.sb.learn_midi = if learning { None } else { Some(slot) };
                self.sb.learn_key = None;
                if self.midi.connected.is_empty() && !learning {
                    self.notify(
                        "Choisissez d'abord un contrôleur MIDI en haut à droite",
                        true,
                    );
                }
            }
            if pad.midi.is_some()
                && w::icon_button(ui, Icon::Close, sq(24.0), BtnStyle::Ghost, true, "Retirer")
                    .clicked()
            {
                pad.midi = None;
            }
        });
        ub.horizontal(|ui| {
            if ui.button("Remplacer le son…").clicked() {
                replace = true;
            }
            if ui.button("Supprimer le pad").clicked() {
                delete = true;
            }
        });

        sanitize(&mut pad, sample.as_deref());
        if delete {
            self.engine.pad(PadCmd::Stop { slot });
            self.board.set(slot, None);
            self.sb.selected = None;
            self.board_modified();
            return;
        }
        if format!("{pad:?}") != before {
            self.board.set(slot, Some(pad));
            self.board_modified();
        }
        if replace && let Some(f) = pick_audio_files(false) {
            self.assign_files(slot, f);
        }
    }

    fn wave_editor(
        &mut self,
        ui: &mut egui::Ui,
        slot: u16,
        pad: &mut Pad,
        sample: Option<&Arc<SampleData>>,
    ) {
        let full = ui.max_rect();
        let rect = Rect::from_min_max(full.min, pos2(full.right(), full.bottom() - 22.0));
        let p = ui.painter_at(full);
        p.rect_filled(rect, CornerRadius::same(t::R), t::BG);
        let Some(s) = sample else {
            p.text(
                rect.center(),
                Align2::CENTER_CENTER,
                "Chargement du son…",
                t::regular(13.0),
                t::TEXT_3,
            );
            return;
        };
        let color = t::color(pad.color as usize);
        let sr = s.sample_rate as f64;
        let frames = s.frames.max(1) as f64;
        let (z0, z1) = self.sb.zoom;
        let v0 = z0 * frames;
        let v1 = z1 * frames;
        let fpp = (v1 - v0) / rect.width() as f64;
        let x_of = |f: f64| rect.left() + ((f - v0) / fpp) as f32;
        let f_of = |x: f32| (v0 + (x - rect.left()) as f64 * fpp).clamp(0.0, frames);

        // Forme d'onde
        let wave = rect.shrink2(vec2(0.0, 18.0));
        let mesh = w::wave_mesh(wave, color.gamma_multiply(0.85), |i| {
            let f0 = v0 + i as f64 * fpp;
            let i0 = (f0 / SAMPLE_PEAK as f64) as usize;
            let i1 = (((f0 + fpp) / SAMPLE_PEAK as f64) as usize)
                .max(i0 + 1)
                .min(s.peaks.len());
            if i0 >= s.peaks.len() {
                return None;
            }
            let (mut mn, mut mx) = (f32::MAX, f32::MIN);
            for pk in &s.peaks[i0..i1] {
                mn = mn.min(pk[0]);
                mx = mx.max(pk[1]);
            }
            Some((mn, mx))
        });
        p.add(egui::Shape::mesh(mesh));
        p.line_segment(
            [
                pos2(rect.left(), wave.center().y),
                pos2(rect.right(), wave.center().y),
            ],
            Stroke::new(1.0, t::SURFACE_3),
        );

        let pr = pad.params(s);
        let (xs, xe) = (x_of(pr.start as f64), x_of(pr.end as f64));
        // Zones hors lecture assombries.
        let shade = Color32::from_black_alpha(165);
        if xs > rect.left() {
            p.rect_filled(
                Rect::from_min_max(rect.min, pos2(xs.min(rect.right()), rect.bottom())),
                CornerRadius::ZERO,
                shade,
            );
        }
        if xe < rect.right() {
            p.rect_filled(
                Rect::from_min_max(pos2(xe.max(rect.left()), rect.top()), rect.max),
                CornerRadius::ZERO,
                shade,
            );
        }
        // Courbes de fondu.
        let top = wave.top();
        let bot = wave.bottom();
        let curve = |a: f32, b: f32, rising: bool| -> Vec<egui::Pos2> {
            (0..=24)
                .map(|k| {
                    let u = k as f32 / 24.0;
                    let g = {
                        let s = (u * std::f32::consts::FRAC_PI_2).sin();
                        s * s
                    };
                    let g = if rising { g } else { 1.0 - g };
                    pos2(a + (b - a) * u, bot - (bot - top) * g)
                })
                .collect()
        };
        let xfi = x_of((pr.start + pr.fade_in) as f64);
        let xfo = x_of((pr.end - pr.fade_out) as f64);
        if pr.fade_in > 0 {
            p.add(egui::Shape::line(
                curve(xs, xfi, true),
                Stroke::new(1.5, t::TEXT),
            ));
        }
        if pr.fade_out > 0 {
            p.add(egui::Shape::line(
                curve(xfo, xe, false),
                Stroke::new(1.5, t::TEXT),
            ));
        }
        // Repères début / fin.
        for (x, lbl) in [(xs, "DÉBUT"), (xe, "FIN")] {
            if x >= rect.left() - 1.0 && x <= rect.right() + 1.0 {
                p.line_segment(
                    [pos2(x, rect.top()), pos2(x, rect.bottom())],
                    Stroke::new(2.0, t::ACCENT),
                );
                let tx = x.clamp(rect.left() + 21.0, rect.right() - 21.0);
                let tab = Rect::from_center_size(pos2(tx, rect.bottom() - 10.0), vec2(42.0, 16.0));
                p.rect_filled(tab, 5.0, t::ACCENT);
                p.text(
                    tab.center(),
                    Align2::CENTER_CENTER,
                    lbl,
                    t::semibold(9.0),
                    Color32::WHITE,
                );
            }
        }
        // Poignées de fondu.
        for x in [xfi, xfo] {
            if x >= rect.left() && x <= rect.right() {
                p.circle_filled(pos2(x, top), 6.0, t::TEXT);
                p.circle_filled(pos2(x, top), 3.0, t::BG);
            }
        }
        // Tête de lecture du pad.
        if let Some(pos) = self.pad_playing(slot) {
            let x = x_of(pos as f64);
            if rect.x_range().contains(x) {
                p.line_segment(
                    [pos2(x, rect.top()), pos2(x, rect.bottom())],
                    Stroke::new(1.5, t::PLAYHEAD),
                );
            }
        }
        // Règle (secondes).
        let secs_vis = (v1 - v0) / sr;
        let step = [0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0]
            .into_iter()
            .find(|st| secs_vis / st <= 12.0)
            .unwrap_or(120.0);
        let mut tt = (v0 / sr / step).ceil() * step;
        while tt * sr <= v1 {
            let x = x_of(tt * sr);
            p.text(
                pos2(x, full.bottom() - 9.0),
                Align2::CENTER_CENTER,
                fmt_secs(tt),
                t::regular(10.0),
                t::TEXT_3,
            );
            tt += step;
        }
        p.text(
            pos2(rect.right() - 10.0, rect.top() + 8.0),
            Align2::RIGHT_TOP,
            format!("Durée jouée {}", fmt_secs((pr.end - pr.start) as f64 / sr)),
            t::medium(10.5),
            t::TEXT_2,
        );
        p.text(
            pos2(rect.right() - 10.0, rect.top() + 22.0),
            Align2::RIGHT_TOP,
            "Ctrl + molette : zoom · double-clic : tout voir",
            t::regular(9.5),
            t::TEXT_3,
        );

        // Interactions
        let resp = ui.interact(
            rect,
            ui.make_persistent_id(("wave", slot)),
            Sense::click_and_drag(),
        );
        let pointer = resp.interact_pointer_pos().or(resp.hover_pos());
        let near = |x: f32, px: f32| (x - px).abs() < 9.0;
        if let Some(pp) = resp.hover_pos() {
            let over_handle = near(xs, pp.x)
                || near(xe, pp.x)
                || (pp.y < top + 14.0 && (near(xfi, pp.x) || near(xfo, pp.x)));
            if over_handle {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
            }
        }
        if resp.drag_started()
            && let Some(pp) = pointer
        {
            self.sb.drag = if pp.y < top + 14.0 && near(xfi, pp.x) {
                Some(Handle::FadeIn)
            } else if pp.y < top + 14.0 && near(xfo, pp.x) {
                Some(Handle::FadeOut)
            } else if near(xs, pp.x) {
                Some(Handle::Start)
            } else if near(xe, pp.x) {
                Some(Handle::End)
            } else {
                None
            };
        }
        if resp.dragged() {
            if let (Some(h), Some(pp)) = (self.sb.drag, pointer) {
                let f = f_of(pp.x) / sr;
                match h {
                    Handle::Start => pad.start = f,
                    Handle::End => pad.end = Some(f),
                    Handle::FadeIn => pad.fade_in = (f - pad.start).max(0.0),
                    Handle::FadeOut => pad.fade_out = (pad.end.unwrap_or(frames / sr) - f).max(0.0),
                }
            } else if self.sb.drag.is_none() {
                // Glisser ailleurs : déplacer la vue zoomée.
                let d = resp.drag_delta().x as f64 * fpp / frames;
                let w = z1 - z0;
                let nz0 = (z0 - d).clamp(0.0, 1.0 - w);
                self.sb.zoom = (nz0, nz0 + w);
            }
        }
        if resp.drag_stopped() {
            self.sb.drag = None;
        }
        if resp.double_clicked() {
            self.sb.zoom = (0.0, 1.0);
        }
        if resp.hovered() {
            let (zoom, sx) = ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta().x));
            if zoom != 1.0 {
                let px = pointer.map_or(0.5, |p| ((p.x - rect.left()) / rect.width()) as f64);
                let anchor = z0 + px * (z1 - z0);
                let w = ((z1 - z0) / zoom as f64).clamp(0.002, 1.0);
                let nz0 = (anchor - px * w).clamp(0.0, 1.0 - w);
                self.sb.zoom = (nz0, nz0 + w);
            }
            if sx != 0.0 {
                let w = z1 - z0;
                let nz0 = (z0 - sx as f64 / rect.width() as f64 * w).clamp(0.0, 1.0 - w);
                self.sb.zoom = (nz0, nz0 + w);
            }
        }
    }
}

/// Garde des réglages cohérents (début < fin, fondus dans la zone jouée).
fn sanitize(pad: &mut Pad, s: Option<&SampleData>) {
    let dur = s.map_or(f64::MAX, |s| s.frames as f64 / s.sample_rate as f64);
    pad.start = pad.start.clamp(0.0, dur);
    if let Some(e) = pad.end {
        let e = e.clamp(pad.start + 0.01, dur);
        pad.end = if (e - dur).abs() < 1e-4 {
            None
        } else {
            Some(e)
        };
    }
    let end = pad.end.unwrap_or(dur);
    let len = (end - pad.start).max(0.0);
    pad.fade_in = pad.fade_in.clamp(0.0, len);
    pad.fade_out = pad.fade_out.clamp(0.0, len - pad.fade_in);
}

fn blend(a: Color32, b: Color32, k: f32) -> Color32 {
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * k) as u8;
    Color32::from_rgb(m(a.r(), b.r()), m(a.g(), b.g()), m(a.b(), b.b()))
}

fn pick_audio_files(multiple: bool) -> Option<Vec<PathBuf>> {
    let d = rfd::FileDialog::new().add_filter("Audio", &crate::soundboard::AUDIO_EXTENSIONS);
    if multiple {
        d.pick_files()
    } else {
        d.pick_file().map(|f| vec![f])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_keeps_valid_ranges() {
        let s = SampleData {
            sample_rate: 1000,
            frames: 10_000,
            data: vec![],
            peaks: vec![],
        };
        let mut p = Pad::new("/x.wav".into(), 0);
        p.start = 8.0;
        p.end = Some(2.0);
        p.fade_in = 5.0;
        p.fade_out = 5.0;
        sanitize(&mut p, Some(&s));
        assert!(p.end.unwrap() > p.start);
        let len = p.end.unwrap() - p.start;
        assert!(p.fade_in + p.fade_out <= len + 1e-9);
    }
}
