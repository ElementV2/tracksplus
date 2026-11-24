//! Onglet Soundboard : grille de pads, pages, contrôleur MIDI, éditeur de son.

use super::multitrack::master_panel;
use super::theme as t;
use super::widgets::{self as w, BtnStyle, Icon};
use crate::app::{App, short};
use crate::midi::{ALL_PORTS, LedMode, LedState, list_out_ports, list_ports};
use crate::sampler::{MAX_PAGES, PADS_PER_PAGE, PadCmd, PadMode, SAMPLE_PEAK, SampleData};
use crate::soundboard::MidiKey;
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

    /// Déplace un pad vers un autre emplacement (échange s'il est occupé).
    pub fn move_pad(&mut self, src: u16, dst: u16) {
        if src == dst {
            return;
        }
        self.engine.pad(PadCmd::Stop { slot: src });
        self.engine.pad(PadCmd::Stop { slot: dst });
        let a = self.board.pad(src).cloned();
        let b = self.board.pad(dst).cloned();
        self.board.set(dst, a);
        self.board.set(src, b);
        if self.sb.selected == Some(src) {
            self.sb.selected = Some(dst);
        } else if self.sb.selected == Some(dst) {
            self.sb.selected = Some(src);
        }
        self.board_modified();
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
        // Uniquement les cases visibles de la grille (pas de pad caché).
        let visible = (self.board.cols * self.board.rows).min(crate::sampler::PADS_PER_PAGE);
        let page_end = page * crate::sampler::PADS_PER_PAGE + visible;
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
            if old.is_some() {
                // L'ancien son s'arrête : le prochain appui lancera bien le nouveau.
                self.engine.pad(PadCmd::Stop { slot: slot as u16 });
            }
            let mut pad = Pad::new(f.clone(), color);
            pad.mode = self.board.default_mode;
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
        self.release_held_keys(ctx);
        // LED du contrôleur : état de chaque pad associé à un contrôle MIDI.
        if self.board.led_mode != LedMode::Off {
            let states: Vec<(MidiKey, LedState)> = self
                .board
                .all_pads()
                .filter_map(|(slot, pad)| {
                    let k = pad.midi?;
                    let color = (pad.color as usize % t::PALETTE.len()) as u8;
                    let st = if self.pad_playing(slot).is_some() {
                        LedState::Playing(color)
                    } else if self.cache.get(&pad.path).is_some() {
                        LedState::Loaded(color)
                    } else {
                        LedState::Off
                    };
                    Some((k, st))
                })
                .collect();
            self.midi.update_leds(self.board.led_mode, &states);
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
            }
            // Les relâchements sont traités dans `release_held_keys` (toujours, quel que
            // soit l'onglet ou les touches de modification).
        }
    }

    /// Relâche les pads « maintenir » : relâchement de touche (même avec Ctrl/Alt ou
    /// depuis un autre onglet), perte de focus de la fenêtre ou bouton souris relâché.
    pub fn release_held_keys(&mut self, ctx: &egui::Context) {
        let (released, focused, mouse_up) = ctx.input(|i| {
            let r: Vec<Key> = i
                .events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Key {
                        key,
                        pressed: false,
                        ..
                    } => Some(*key),
                    _ => None,
                })
                .collect();
            (r, i.focused, !i.pointer.primary_down())
        });
        for key in released {
            if let Some(slot) = self.sb.held_keys.remove(&key) {
                self.release_pad(slot);
            }
        }
        if !focused {
            let held: Vec<u16> = self.sb.held_keys.drain().map(|(_, s)| s).collect();
            for slot in held {
                self.release_pad(slot);
            }
        }
        if (!focused || mouse_up)
            && let Some(slot) = self.sb.mouse_down.take()
        {
            self.release_pad(slot);
        }
    }

    // ---------------------------------------------------------------- interface

    pub fn soundboard_ui(&mut self, ui: &mut egui::Ui) {
        self.soundboard_keys(ui.ctx());
        if self.sb.page >= self.board.pages.len() {
            self.sb.page = 0;
        }
        egui::Panel::top("sb_head")
            .exact_size(36.0)
            .frame(
                egui::Frame::new()
                    .fill(t::SURFACE_2)
                    .inner_margin(egui::Margin::symmetric(10, 0)),
            )
            .show(ui, |ui| self.sb_header(ui));
        egui::Panel::right("sb_master")
            .exact_size(86.0)
            .frame(egui::Frame::new().fill(t::SURFACE))
            .show(ui, |ui| {
                let mut m = self.master_pad;
                master_panel(ui, "PADS", &mut m);
                self.master_pad = m;
            });
        // Le pad sélectionné doit être sur la page affichée.
        if self
            .sb
            .selected
            .is_some_and(|s| s as usize / PADS_PER_PAGE != self.sb.page)
        {
            self.sb.selected = None;
        }
        self.confirm_delete_page_dialog(ui.ctx());
        if self.sb.selected.and_then(|s| self.board.pad(s)).is_some() {
            egui::Panel::bottom("sb_editor")
                .resizable(true)
                .default_size(300.0)
                .min_size(240.0)
                .max_size(520.0)
                .frame(
                    egui::Frame::new()
                        .fill(t::SURFACE)
                        .inner_margin(egui::Margin::same(12)),
                )
                .show(ui, |ui| {
                    let r = ui.max_rect();
                    w::hline(
                        ui.painter(),
                        r.left() - 12.0,
                        r.right() + 12.0,
                        r.top() - 12.0,
                        t::LINE,
                    );
                    self.pad_editor(ui)
                });
        }
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(t::BG)
                    .inner_margin(egui::Margin::same(6)),
            )
            .show(ui, |ui| self.pad_grid(ui));
    }

    /// Mode commun aux pads de la page (None si mélangé).
    fn page_mode(&self) -> Option<PadMode> {
        let mut modes = self.board.pages[self.sb.page]
            .pads
            .iter()
            .flatten()
            .map(|p| p.mode);
        let first = modes.next()?;
        modes.all(|m| m == first).then_some(first)
    }

    /// Suppression d'une page : confirmation, arrêt des pads concernés (les pages
    /// suivantes changent de numéro de case).
    fn confirm_delete_page_dialog(&mut self, ctx: &egui::Context) {
        let Some(i) = self.sb.confirm_delete_page else {
            return;
        };
        if i >= self.board.pages.len() || self.board.pages.len() <= 1 {
            self.sb.confirm_delete_page = None;
            return;
        }
        let n = self.board.pages[i].pads.iter().flatten().count();
        let name = self.board.pages[i].name.clone();
        let mut keep = false;
        let mut delete = false;
        egui::Modal::new(egui::Id::unique("confirm_del_page"))
            .frame(t::window_frame())
            .show(ctx, |ui| {
                ui.set_width(380.0);
                ui.label(egui::RichText::new("SUPPRIMER LA PAGE").font(t::semibold(11.5)));
                ui.add_space(8.0);
                ui.label(format!(
                    "La page « {name} » et ses {n} pad(s) seront supprimés."
                ));
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.button("Annuler").clicked() {
                        keep = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new("Supprimer").color(Color32::BLACK),
                                )
                                .fill(t::REC),
                            )
                            .clicked()
                        {
                            delete = true;
                        }
                    });
                });
            });
        if keep {
            self.sb.confirm_delete_page = None;
        }
        if delete {
            self.sb.confirm_delete_page = None;
            // Arrêt des sons des pages renumérotées et remise à zéro des états liés aux cases.
            for pg in i..self.board.pages.len() {
                for k in 0..PADS_PER_PAGE {
                    self.engine.pad(PadCmd::Stop {
                        slot: Board::slot(pg, k),
                    });
                }
            }
            self.sb.held_keys.clear();
            self.sb.mouse_down = None;
            self.sb.learn_key = None;
            self.sb.learn_midi = None;
            self.sb.drag_pad = None;
            self.board.pages.remove(i);
            self.sb.page = self.sb.page.min(self.board.pages.len() - 1);
            self.sb.selected = None;
            self.board_modified();
        }
    }

    fn sb_header(&mut self, ui: &mut egui::Ui) {
        let full = ui.max_rect();
        w::hline(
            ui.painter(),
            full.left() - 10.0,
            full.right() + 10.0,
            full.bottom() - 1.0,
            t::LINE,
        );
        ui.horizontal_centered(|ui| {
            // Pages
            let mut page = self.sb.page;
            let names: Vec<String> = self.board.pages.iter().map(|p| p.name.to_uppercase()).collect();
            let refs: Vec<&str> = names.iter().map(String::as_str).collect();
            let before = ui.cursor().left();
            ui.scope(|ui| {
                ui.set_height(full.height());
                let (changed, rects) = w::view_tabs_rects(ui, &mut page, &refs);
                if changed {
                    self.sb.page = page;
                }
                self.sb.page_tabs = rects;
            });
            // Menu contextuel de la page active (renommer / supprimer).
            let tabs_rect = Rect::from_min_max(pos2(before, full.top()), pos2(ui.cursor().left(), full.bottom()));
            let mut remove_page = None;
            ui.interact(tabs_rect, ui.make_persistent_id("page_tabs_ctx"), Sense::click())
                .on_hover_text("Clic droit : renommer ou supprimer la page")
                .context_menu(|ui| {
                    let i = self.sb.page;
                    ui.set_width(200.0);
                    if ui.add(egui::TextEdit::singleline(&mut self.board.pages[i].name).desired_width(180.0)).changed() {
                        self.board_changed = Some(self.now);
                    }
                    if self.board.pages.len() > 1 && ui.button("Supprimer la page").clicked() {
                        remove_page = Some(i);
                        ui.close();
                    }
                });
            if let Some(i) = remove_page {
                self.sb.confirm_delete_page = Some(i);
            }
            if self.board.pages.len() < MAX_PAGES && w::icon_button(ui, Icon::Plus, 22.0, BtnStyle::Ghost, true, "Nouvelle page").clicked() {
                let n = self.board.pages.len() + 1;
                self.board.pages.push(Page::new(format!("Page {n}")));
                self.sb.page = self.board.pages.len() - 1;
                self.board_modified();
            }

            ui.add_space(12.0);
            if w::tool_button(ui, Some(Icon::Follow), "Déplacer", self.sb.move_mode, true)
                .on_hover_text("Glisser les pads pour les ranger (sans les déclencher).\nDépôt sur un pad : échange. Dépôt sur un onglet de page : envoi vers cette page.\nAstuce : Alt + glisser fonctionne aussi hors de ce mode.")
                .clicked()
            {
                self.sb.move_mode = !self.sb.move_mode;
            }
            ui.add_space(12.0);
            w::section(ui, "Mode");
            let mut mode = self.page_mode().unwrap_or(self.board.default_mode);
            let mixed = self.page_mode().is_none() && self.board.pages[self.sb.page].pads.iter().flatten().next().is_some();
            let r = ui.scope(|ui| {
                w::segmented(ui, "pagemode", &mut mode, &[(PadMode::Toggle, "▶ / ■  Lecture-Stop"), (PadMode::Gate, "Maintenir")])
            });
            r.response.on_hover_text(
                "Lecture-Stop : un appui lance, un nouvel appui arrête.\nMaintenir : le son joue tant que le pad est tenu, et s'arrête au relâchement.\nS'applique à tous les pads de la page et aux nouveaux pads.",
            );
            if r.inner {
                self.board.default_mode = mode;
                for p in self.board.pages[self.sb.page].pads.iter_mut().flatten() {
                    p.mode = mode;
                }
                self.board_modified();
            }
            if mixed {
                ui.label(egui::RichText::new("mixte").font(t::regular(10.5)).color(t::TEXT_3));
            }
        });

        let right = Rect::from_min_max(pos2(full.right() - 700.0, full.top()), full.max);
        let mut child = ui.new_child(
            UiBuilder::new()
                .max_rect(right)
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
        );
        let ui = &mut child;
        ui.spacing_mut().item_spacing.x = 6.0;
        {
            let g = ui
                .painter()
                .layout_no_wrap("Tout arrêter".into(), t::semibold(11.5), t::TEXT);
            let (r, resp) = ui.allocate_exact_size(vec2(g.size().x + 34.0, 24.0), Sense::click());
            let bg = if resp.hovered() {
                t::REC
            } else {
                t::mix(t::REC, t::SURFACE_2, 0.45)
            };
            ui.painter().rect_filled(r, t::R, bg);
            w::paint_icon(
                ui.painter(),
                Icon::Stop,
                pos2(r.left() + 13.0, r.center().y),
                4.5,
                Color32::BLACK,
            );
            ui.painter().galley(
                pos2(r.left() + 23.0, r.center().y - g.size().y / 2.0),
                g,
                Color32::BLACK,
            );
            if resp
                .on_hover_text(format!(
                    "Arrêt de tous les pads en fondu de {:.1} s (Échap)",
                    self.board.panic_fade
                ))
                .clicked()
            {
                self.stop_all_pads();
            }
        }

        let mut db = self.board.master_db;
        ui.label(
            egui::RichText::new(format!("{} dB", w::fmt_db(db)))
                .font(t::medium(10.5))
                .color(t::TEXT_2),
        );
        let r = ui.add_sized(
            [110.0, 20.0],
            egui::Slider::new(&mut db, -40.0..=6.0)
                .show_value(false)
                .trailing_fill(true),
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
        let (ir, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
        w::paint_icon(ui.painter(), Icon::Speaker, ir.center(), 5.5, t::TEXT_2);
        ui.add_space(6.0);

        // Sortie du soundboard : n'importe quelle paire de n'importe quel périphérique.
        let main_dev = self.engine.status.output.clone();
        let cur_dev = self.board.out_device.clone().or_else(|| main_dev.clone());
        let ch_of = |name: &str| {
            self.out_devices
                .iter()
                .find(|d| d.name == name)
                .map_or(2, |d| d.channels)
        };
        let cur = cur_dev.as_deref().map_or("Pas de sortie".to_string(), |d| {
            crate::app::out_dest_label(d, ch_of(d), self.board.out_left)
        });
        let mut pick: Option<(String, u16)> = None;
        egui::ComboBox::from_id_salt("sb_out")
            .width(220.0)
            .height(360.0)
            .selected_text(short(&cur, 30))
            .show_ui(ui, |ui| {
                for d in &self.out_devices {
                    for l in (0..d.channels.max(2)).step_by(2) {
                        let sel = cur_dev.as_deref() == Some(&d.name) && self.board.out_left == l;
                        if ui
                            .selectable_label(
                                sel,
                                crate::app::out_dest_label(&d.name, d.channels, l),
                            )
                            .clicked()
                        {
                            pick = Some((d.name.clone(), l));
                        }
                    }
                }
            })
            .response
            .on_hover_text(cur.clone());
        if let Some((dev, l)) = pick {
            let reopen = cur_dev.as_deref() != Some(dev.as_str());
            if reopen && self.is_recording() {
                self.notify(
                    "Changement de sortie du soundboard impossible pendant un enregistrement",
                    true,
                );
            } else {
                // Même périphérique que l'écoute : un seul flux de sortie.
                self.board.out_device = (main_dev.as_deref() != Some(dev.as_str())).then_some(dev);
                self.board.out_left = l;
                self.board_changed = Some(self.now);
                if reopen {
                    if let Err(e) = self.apply_audio(self.prefs.clone()) {
                        self.notify(e, true);
                    }
                } else {
                    self.engine.pad(PadCmd::SetOutput { left: l as usize });
                }
            }
        }

        let active = self.now - self.sb.midi_seen < 0.15;
        let cur = self
            .board
            .midi_port
            .clone()
            .unwrap_or_else(|| "Aucun contrôleur".into());
        let mut choose: Option<Option<String>> = None;
        let combo = egui::ComboBox::from_id_salt("sb_midi")
            .width(160.0)
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
        let (dr, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
        let dot = if active {
            t::PLAY
        } else if self.midi.connected.is_empty() {
            t::SURFACE_4
        } else {
            t::mix(t::PLAY, t::SURFACE_2, 0.6)
        };
        ui.painter().rect_filled(
            Rect::from_center_size(dr.center(), vec2(6.0, 6.0)),
            1.0,
            dot,
        );
        let tip = match (&self.midi.error, &self.sb.midi_last) {
            (Some(e), _) => format!("Erreur MIDI : {e}"),
            (None, Some(l)) => format!("Dernier message : {l}"),
            _ => "Contrôleur MIDI USB (Launchpad, APC, nanoPAD…)".into(),
        };
        combo.response.on_hover_text(tip);
        if let Some(c) = choose {
            self.board.midi_port = c.clone();
            self.midi.leds_off(self.board.led_mode);
            self.midi.connect(c.as_deref());
            let led_port = self.board.led_port.clone();
            self.midi.connect_leds(led_port.as_deref());
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
        ui.label(
            egui::RichText::new("MIDI")
                .font(t::caps(10.0))
                .color(t::TEXT_3),
        );
        ui.add_space(6.0);
        // LED du contrôleur
        let led_on = self.board.led_mode != LedMode::Off && !self.midi.led_ports().is_empty();
        let led_label = match self.board.led_mode {
            LedMode::Off => "LED off".to_string(),
            _ if self.midi.led_ports().is_empty() => "LED —".to_string(),
            LedMode::Auto => "LED auto".to_string(),
            _ => "LED on".to_string(),
        };
        let mut led_change: Option<(Option<LedMode>, Option<Option<String>>)> = None;
        let led_combo = egui::ComboBox::from_id_salt("sb_led")
            .width(74.0)
            .selected_text(led_label)
            .show_ui(ui, |ui| {
                ui.set_min_width(240.0);
                w::section(ui, "Allumage des LED");
                for m in LedMode::ALL {
                    if ui
                        .selectable_label(self.board.led_mode == m, m.label())
                        .clicked()
                    {
                        led_change = Some((Some(m), None));
                    }
                }
                ui.separator();
                w::section(ui, "Sortie MIDI des LED");
                if ui
                    .selectable_label(
                        self.board.led_port.is_none(),
                        "Même contrôleur que l'entrée",
                    )
                    .clicked()
                {
                    led_change = Some((None, Some(None)));
                }
                for p in list_out_ports() {
                    if ui
                        .selectable_label(self.board.led_port.as_deref() == Some(&p), &p)
                        .clicked()
                    {
                        led_change = Some((None, Some(Some(p))));
                    }
                }
            });
        led_combo.response.on_hover_text(if led_on {
            format!("LED envoyées à : {}", self.midi.led_ports().join(", "))
        } else {
            "Aucune sortie LED : choisissez un contrôleur MIDI, ou sa sortie dans ce menu".into()
        });
        if let Some((mode, port)) = led_change {
            self.midi.leds_off(self.board.led_mode);
            if let Some(m) = mode {
                self.board.led_mode = m;
            }
            if let Some(p) = port {
                self.board.led_port = p;
            }
            let lp = self.board.led_port.clone();
            self.midi.connect_leds(lp.as_deref());
            self.board_changed = Some(self.now);
        }
        ui.add_space(6.0);

        let (c, r) = (self.board.cols, self.board.rows);
        egui::ComboBox::from_id_salt("sb_grid")
            .width(64.0)
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
        let gap = 4.0;
        let pw = ((area.width() - gap * (cols as f32 - 1.0)) / cols as f32).max(40.0);
        let ph =
            ((area.height() - gap * (rows as f32 - 1.0)) / rows as f32).clamp(40.0, pw.max(80.0));
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
        let moving = self.sb.move_mode || ui.input(|i| i.modifiers.alt);
        let mut pad_rects: Vec<(u16, Rect)> = Vec::new();

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
            pad_rects.push((slot, rect));
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
            let cr = CornerRadius::same(t::R);
            let Some(pad) = self.board.pad(slot).cloned() else {
                p.rect_filled(
                    rect,
                    cr,
                    if resp.hovered() {
                        t::SURFACE_2
                    } else {
                        t::SURFACE
                    },
                );
                if hovered_drop {
                    p.rect_stroke(rect, cr, Stroke::new(1.0, t::ACCENT), StrokeKind::Inside);
                }
                if resp.hovered() || hovered_drop {
                    w::paint_icon(
                        p,
                        Icon::Plus,
                        rect.center() - vec2(0.0, 6.0),
                        5.0,
                        t::TEXT_3,
                    );
                    p.text(
                        rect.center() + vec2(0.0, 10.0),
                        Align2::CENTER_CENTER,
                        "Déposer un son",
                        t::regular(10.0),
                        t::TEXT_3,
                    );
                }
                p.text(
                    rect.right_bottom() + vec2(-6.0, -6.0),
                    Align2::RIGHT_BOTTOM,
                    format!("{}", idx + 1),
                    t::regular(9.0),
                    t::SURFACE_4,
                );
                if resp.clicked() {
                    empty_clicked = Some(slot);
                }
                continue;
            };

            // Déclenchement à l'appui (pas au relâchement) pour la réactivité.
            // En mode « déplacer » (ou avec Alt), l'appui sélectionne seulement.
            if !blocked && press_pos.is_some_and(|p| rect.contains(p)) {
                self.sb.selected = Some(slot);
                if !moving {
                    self.sb.mouse_down = Some(slot);
                    self.press_pad(slot);
                }
            }
            if moving && resp.drag_started() {
                self.sb.drag_pad = Some(slot);
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

            let color = t::PALETTE[pad.color as usize % t::PALETTE.len()];
            let playing = self.pad_playing(slot);
            let flash = self
                .sb
                .flash
                .get(&slot)
                .is_some_and(|&f| self.now - f < 0.10);
            let selected = self.sb.selected == Some(slot);
            let sample = self.cache.get(&pad.path).cloned();
            let p = ui.painter();

            let body = if playing.is_some() {
                t::mix(t::SURFACE, color, 0.62)
            } else if flash {
                t::mix(t::SURFACE, color, 0.45)
            } else if resp.hovered() {
                t::mix(t::SURFACE, color, 0.26)
            } else {
                t::mix(t::SURFACE, color, 0.18)
            };
            p.rect_filled(rect, cr, body);
            p.rect_filled(
                Rect::from_min_max(rect.min, pos2(rect.right(), rect.top() + 3.0)),
                CornerRadius {
                    nw: t::R,
                    ne: t::R,
                    sw: 0,
                    se: 0,
                },
                color,
            );
            let fg = if playing.is_some() {
                Color32::from_black_alpha(230)
            } else {
                t::TEXT
            };
            let fg2 = if playing.is_some() {
                Color32::from_black_alpha(170)
            } else {
                t::TEXT_2
            };

            // Icône d'état : ▶ (prêt), ■ (en lecture, appuyer pour arrêter), main (maintien).
            let ic = pos2(rect.left() + 13.0, rect.top() + 17.0);
            let icon = match (pad.mode, playing.is_some()) {
                (PadMode::Gate, _) => Icon::Hold,
                (PadMode::Toggle, true) => Icon::Stop,
                (PadMode::Toggle, false) => Icon::Play,
            };
            w::paint_icon(
                p,
                icon,
                ic,
                4.5,
                if playing.is_some() {
                    Color32::BLACK
                } else {
                    color
                },
            );
            p.text(
                rect.right_top() + vec2(-7.0, 9.0),
                Align2::RIGHT_TOP,
                format!("{}", idx + 1),
                t::regular(9.0),
                fg2,
            );

            let name_rect = Rect::from_min_max(
                pos2(rect.left() + 24.0, rect.top() + 10.0),
                pos2(rect.right() - 20.0, rect.bottom() - 22.0),
            );
            let mut job = egui::text::LayoutJob::simple(
                pad.name.clone(),
                t::semibold(12.0),
                fg,
                name_rect.width(),
            );
            job.wrap.max_rows = if ph > 90.0 { 3 } else { 2 };
            let gal = p.layout_job(job);
            p.galley(name_rect.min, gal, fg);

            let by = rect.bottom() - 11.0;
            let (status, scol) = match self.cache.map.get(&pad.path) {
                Some(LoadState::Ready(s)) => {
                    let pr = pad.params(s);
                    let len = (pr.end - pr.start) as f64 / s.sample_rate as f64;
                    match playing {
                        Some(pos) => {
                            let rem = (pr.end as f64 - pos as f64).max(0.0) / s.sample_rate as f64;
                            (format!("−{}", fmt_secs(rem)), fg)
                        }
                        None => (fmt_secs(len), fg2),
                    }
                }
                Some(LoadState::Failed(_)) => ("Fichier illisible".into(), t::REC),
                _ => ("Chargement…".into(), t::TEXT_3),
            };
            p.text(
                pos2(rect.left() + 8.0, by),
                Align2::LEFT_CENTER,
                status,
                t::medium(10.0),
                scol,
            );
            let mut bx = rect.right() - 6.0;
            let mut badge = |txt: String| {
                let g = p.layout_no_wrap(txt, t::semibold(9.0), t::TEXT_2);
                let r = Rect::from_min_size(
                    pos2(bx - g.size().x - 8.0, by - 7.0),
                    vec2(g.size().x + 8.0, 14.0),
                );
                p.rect_filled(r, t::R, Color32::from_black_alpha(110));
                p.galley(
                    pos2(r.left() + 4.0, r.center().y - g.size().y / 2.0),
                    g,
                    t::TEXT_2,
                );
                bx = r.left() - 3.0;
            };
            if let Some(m) = pad.midi {
                badge(m.label());
            }
            if let Some(k) = &pad.key {
                badge(k.clone());
            }
            if pad.looped {
                badge("BOUCLE".into());
            }
            if let (Some(pos), Some(s)) = (playing, sample) {
                let pr = pad.params(&s);
                let f = ((pos as f64 - pr.start as f64) / (pr.end - pr.start).max(1) as f64)
                    .clamp(0.0, 1.0) as f32;
                let bar = Rect::from_min_max(pos2(rect.left(), rect.bottom() - 3.0), rect.max);
                p.rect_filled(bar, 0.0, Color32::from_black_alpha(90));
                p.rect_filled(
                    Rect::from_min_size(bar.min, vec2(bar.width() * f, bar.height())),
                    0.0,
                    Color32::from_black_alpha(220),
                );
            }
            if selected {
                p.rect_stroke(
                    rect,
                    cr,
                    Stroke::new(1.0, Color32::WHITE),
                    StrokeKind::Inside,
                );
            }
            if hovered_drop {
                p.rect_stroke(rect, cr, Stroke::new(1.5, t::ACCENT), StrokeKind::Inside);
            }
            if (self.sb.learn_midi == Some(slot) || self.sb.learn_key == Some(slot))
                && (self.now * 3.0).fract() < 0.5
            {
                p.rect_stroke(rect, cr, Stroke::new(2.0, t::ACCENT), StrokeKind::Inside);
            }
        }

        if let (Some(slot), true) = (self.sb.mouse_down, primary_released) {
            self.release_pad(slot);
            self.sb.mouse_down = None;
        }
        // Déplacement d'un pad : cible survolée, fantôme sous le pointeur, dépôt.
        if let Some(src) = self.sb.drag_pad {
            let target =
                pointer.and_then(|pp| pad_rects.iter().find(|(_, r)| r.contains(pp)).copied());
            let tab_target =
                pointer.and_then(|pp| self.sb.page_tabs.iter().position(|r| r.contains(pp)));
            let p = ui.ctx().layer_painter(egui::LayerId::new(
                egui::Order::Tooltip,
                egui::Id::unique("pad_drag"),
            ));
            if let Some((s, r)) = target
                && s != src
            {
                p.rect_stroke(r, t::R, Stroke::new(2.0, t::ACCENT), StrokeKind::Inside);
            }
            if let Some(i) = tab_target
                && let Some(r) = self.sb.page_tabs.get(i)
            {
                p.rect_stroke(*r, t::R, Stroke::new(1.5, t::ACCENT), StrokeKind::Inside);
            }
            if let (Some(pp), Some(pad)) = (pointer, self.board.pad(src)) {
                let color = t::PALETTE[pad.color as usize % t::PALETTE.len()];
                let g = p.layout_no_wrap(pad.name.clone(), t::semibold(11.0), Color32::BLACK);
                let r = Rect::from_min_size(pp + vec2(12.0, 8.0), vec2(g.size().x + 16.0, 22.0));
                p.rect_filled(r, t::R, color);
                p.galley(
                    pos2(r.left() + 8.0, r.center().y - g.size().y / 2.0),
                    g,
                    Color32::BLACK,
                );
            }
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            if primary_released {
                self.sb.drag_pad = None;
                if let Some((dst, _)) = target.filter(|(d, _)| *d != src) {
                    self.move_pad(src, dst);
                } else if let Some(pg) = tab_target.filter(|&pg| pg != self.sb.page) {
                    let free = (0..PADS_PER_PAGE)
                        .map(|i| Board::slot(pg, i))
                        .find(|s| self.board.pad(*s).is_none());
                    match free {
                        Some(dst) => {
                            self.move_pad(src, dst);
                            let name = self.board.pages[pg].name.clone();
                            self.notify(format!("Pad envoyé vers « {name} »"), false);
                        }
                        None => self.notify("Cette page est pleine", true),
                    }
                }
            }
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
        w::segmented(
            &mut ua,
            "padmode",
            &mut pad.mode,
            &[
                (PadMode::Toggle, "▶ / ■  Lecture-Stop"),
                (PadMode::Gate, "Maintenir pour jouer"),
            ],
        );
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
            if w::tool_button(ui, Some(Icon::Play), "Écouter", false, sample.is_some()).clicked() {
                self.press_pad(slot);
            }
            if w::tool_button(ui, Some(Icon::Stop), "Stop", false, true).clicked() {
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
                && w::icon_button(ui, Icon::Close, 24.0, BtnStyle::Ghost, true, "Retirer").clicked()
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
                && w::icon_button(ui, Icon::Close, 24.0, BtnStyle::Ghost, true, "Retirer").clicked()
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
        let color = t::PALETTE[pad.color as usize % t::PALETTE.len()];
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
    pad.start = pad.start.clamp(0.0, (dur - 0.01).max(0.0));
    if let Some(e) = pad.end {
        let lo = (pad.start + 0.01).min(dur);
        let e = e.clamp(lo, dur);
        pad.end = if (e - dur).abs() < 1e-4 {
            None
        } else {
            Some(e)
        };
    }
    let end = pad.end.unwrap_or(dur);
    let len = (end - pad.start).max(0.0);
    pad.fade_in = pad.fade_in.clamp(0.0, len);
    pad.fade_out = pad.fade_out.clamp(0.0, (len - pad.fade_in).max(0.0));
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
