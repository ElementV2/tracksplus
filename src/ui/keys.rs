//! Réglages → Raccourcis : chaque action, ses deux raccourcis possibles, à changer en
//! cliquant puis en appuyant sur la nouvelle combinaison. Appliqué et enregistré aussitôt.

use super::theme as t;
use crate::app::App;
use crate::shortcuts::{self as k, Combo, DEFS, MOUSE};
use eframe::egui::{self, vec2};

/// Colonne des libellés et largeur d'un raccourci : 250 + 2 × 112 + boutons + barre de
/// défilement tiennent dans la largeur des réglages (`SETTINGS_W`).
const LABEL_W: f32 = 250.0;
const KEY_W: f32 = 112.0;

impl App {
    /// `bottom` : bas commun des onglets des réglages (le bouton s'y aligne, comme
    /// « Appliquer » ; la liste défile dans l'espace restant).
    pub fn keys_settings(&mut self, ui: &mut egui::Ui, bottom: f32) {
        self.capture_key(ui);
        ui.label(
            egui::RichText::new(
                "Clic : changer · clic droit : retirer · Échap : annuler",
            )
            .font(t::regular(11.5))
            .color(t::TEXT_3),
        );
        ui.add_space(6.0);
        let mut reset_all = false;
        // Place laissée au bouton du bas (même ligne qu'« Appliquer »).
        let list_h = (bottom - 26.0 - 14.0 - ui.cursor().top()).max(80.0);
        egui::ScrollArea::vertical()
            .max_height(list_h)
            .min_scrolled_height(list_h)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let mut group = "";
                for d in DEFS {
                    if d.group != group {
                        group = d.group;
                        ui.add_space(if group == DEFS[0].group { 0.0 } else { 10.0 });
                        ui.label(
                            egui::RichText::new(group.to_uppercase())
                                .font(t::semibold(10.5))
                                .color(t::TEXT_3),
                        );
                        ui.add_space(2.0);
                    }
                    self.key_row(ui, d);
                }
                ui.add_space(10.0);
                ui.label(
                    egui::RichText::new("SOURIS")
                        .font(t::semibold(10.5))
                        .color(t::TEXT_3),
                );
                ui.add_space(2.0);
                egui::Grid::new("mouse_keys")
                    .num_columns(2)
                    .spacing([16.0, 6.0])
                    .show(ui, |ui| {
                        for (gesture, what) in MOUSE {
                            left_label(ui, what, 18.0);
                            ui.label(egui::RichText::new(*gesture).color(t::TEXT_3));
                            ui.end_row();
                        }
                    });
            });
        ui.add_space((bottom - 26.0 - ui.cursor().top()).max(0.0));
        // Ligne de 26 points, comme celle d'« Appliquer » : même bas de fenêtre.
        ui.allocate_ui_with_layout(
            vec2(ui.available_width(), 26.0),
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| {
            if ui
                .add_enabled(
                    self.keys != k::Keymap::default(),
                    egui::Button::new("Tout remettre par défaut"),
                )
                .clicked()
            {
                reset_all = true;
            }
            },
        );
        if reset_all {
            self.keys.reset_all();
            self.keys_capture = None;
            self.save_keys();
        }
    }

    /// Une ligne : libellé, deux emplacements de raccourci, retour à la valeur par défaut.
    fn key_row(&mut self, ui: &mut egui::Ui, d: &k::Def) {
        ui.horizontal(|ui| {
            left_label(ui, d.label, 22.0);
            let combos: Vec<Combo> = self.keys.combos(d.id).to_vec();
            // Deux emplacements ; le second n'apparaît qu'une fois le premier rempli.
            for slot in 0..2 {
                if slot > combos.len() {
                    break;
                }
                let capturing = self.keys_capture == Some((d.id, slot));
                let (text, col) = match (capturing, combos.get(slot)) {
                    (true, _) => ("Appuyez…".to_string(), t::ACCENT),
                    (false, Some(c)) => (c.label(), t::TEXT),
                    (false, None) => ("+".to_string(), t::TEXT_3),
                };
                // Largeurs fixes : la ligne ne change jamais de taille.
                let w = if combos.get(slot).is_none() && !capturing {
                    28.0
                } else {
                    KEY_W
                };
                let btn = egui::Button::new(
                    egui::RichText::new(text).color(col).font(t::regular(12.0)),
                )
                .truncate();
                let r = ui.add_sized(vec2(w, 22.0), btn);
                let r = if combos.get(slot).is_some() {
                    r.on_hover_text("Clic : changer · clic droit : retirer")
                } else {
                    r.on_hover_text("Ajouter un raccourci")
                };
                if r.clicked() {
                    self.keys_capture = if capturing { None } else { Some((d.id, slot)) };
                }
                if r.secondary_clicked() && combos.get(slot).is_some() {
                    self.keys.clear(d.id, slot);
                    self.keys_capture = None;
                    self.save_keys();
                }
            }
            if !self.keys.is_default(d.id)
                && ui
                    .small_button("↺")
                    .on_hover_text(format!(
                        "Par défaut : {}",
                        default_label(d)
                    ))
                    .clicked()
            {
                self.keys.reset(d.id);
                self.keys_capture = None;
                self.save_keys();
            }
        });
    }

    /// Raccourci en cours d'attribution : la première touche appuyée (avec ses
    /// modificateurs) lui est donnée ; Échap seul annule. Les touches sont retirées des
    /// événements, rien d'autre ne les reçoit.
    fn capture_key(&mut self, ui: &mut egui::Ui) {
        let Some((id, slot)) = self.keys_capture else {
            return;
        };
        let pressed = ui.input_mut(|i| {
            let mut got = None;
            i.events.retain(|e| match e {
                egui::Event::Key {
                    key,
                    pressed,
                    modifiers,
                    ..
                } => {
                    if *pressed && got.is_none() {
                        got = Some(Combo::new(*key, *modifiers));
                    }
                    false
                }
                _ => true,
            });
            got
        });
        let Some(c) = pressed else {
            return;
        };
        self.keys_capture = None;
        if c.key == egui::Key::Escape && !c.ctrl && !c.shift && !c.alt {
            return;
        }
        if !k::assignable(&c) {
            self.notify(format!("{} : touche réservée", c.label()), true);
            return;
        }
        let taken = self.keys.set(id, slot, c);
        self.save_keys();
        if !taken.is_empty() {
            self.notify(
                format!("{} retiré de « {} »", c.label(), taken.join(" », « ")),
                false,
            );
        }
    }

    fn save_keys(&mut self) {
        if let Err(e) = self.keys.save() {
            self.notify(format!("Raccourcis non enregistrés : {e:#}"), true);
        }
    }
}

/// Libellé aligné à gauche dans une colonne de largeur fixe (coupé avec « … » si trop long).
fn left_label(ui: &mut egui::Ui, text: &str, h: f32) {
    ui.allocate_ui_with_layout(
        vec2(LABEL_W, h),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_min_size(vec2(LABEL_W, h));
            ui.add(egui::Label::new(egui::RichText::new(text).color(t::TEXT_2)).truncate());
        },
    );
}

/// Raccourcis par défaut d'une action, en clair.
fn default_label(d: &k::Def) -> String {
    let v: Vec<String> = d
        .defaults
        .iter()
        .filter_map(|s| Combo::parse(s))
        .map(|c| c.label())
        .collect();
    if v.is_empty() {
        "aucun".into()
    } else {
        v.join(", ")
    }
}
