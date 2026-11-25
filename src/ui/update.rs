//! Interface des mises à jour : pastille dans la barre du haut quand une version est
//! disponible, entrée du menu, fenêtre de mise à jour (notes, téléchargement, installation).

use crate::app::App;
use crate::ui::theme as t;
use crate::ui::widgets::{self as w, Icon};
use crate::update::{self, State};
use eframe::egui::{self, Color32, CornerRadius, Sense, Stroke, StrokeKind, pos2, vec2};

impl App {
    /// Recherche automatique, quelques secondes après le lancement (une fois).
    pub fn update_background(&mut self, ctx: &egui::Context) {
        if !self.update_checked && self.now > 3.0 {
            self.update_checked = true;
            // Installeurs d'un lancement précédent : plus utiles (aucun téléchargement n'a
            // pu commencer avant ce premier passage).
            if !self.updater.busy() && !matches!(self.updater.state(), State::Ready { .. }) {
                update::clean_downloads();
            }
            if self.update_settings.auto_check {
                self.updater.check(ctx);
            }
        }
        // Le spectacle a repris pendant le téléchargement (lecture, prise, pad) : jamais
        // de fermeture surprise. L'installation attend un nouveau clic dans la fenêtre.
        // (Une prise en cours de finalisation n'annule rien : on attend simplement qu'elle
        // soit terminée, plus bas.)
        let active = !matches!(self.transport, crate::app::Transport::Stopped);
        if self.update_install && (active || self.pads_status().0 > 0) {
            self.update_install = false;
            self.notify("Mise à jour reportée", false);
            return;
        }
        // Installeur prêt après « Installer et redémarrer » : on le lance et on ferme.
        if self.update_install
            && let State::Ready { file, release } = self.updater.state()
        {
            if !self.is_stopped()
                || self.save_as_job.is_some()
                || self.import_job.is_some()
                || self.export_job.is_some()
                || self.audio_import.is_some()
            {
                // Prise, finalisation, copie ou export en cours : on réessaie dès que c'est fini.
                return;
            }
            self.update_install = false;
            // Tout est sauvegardé avant que l'installeur ne ferme l'application ; sinon
            // (disque plein, clé retirée) on ne ferme pas.
            self.board_changed = Some(self.now);
            if !self.save_before(
                "Session non sauvegardée : installation annulée. Recliquez « Installer » \
                 dans les 10 s pour installer quand même (modifications non sauvegardées perdues).",
            ) {
                return;
            }
            // Empreinte revérifiée au lancement (le fichier a pu attendre des heures).
            match update::launch_installer(&file, release.sha256.as_deref()) {
                Ok(()) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                Err(e) => self.updater.set(State::Failed(e)),
            }
        }
    }

    /// Version proposée dans la barre du haut (pas celle que l'utilisateur a ignorée).
    fn offered_version(&self) -> Option<String> {
        match self.updater.state() {
            State::Available(r) if self.update_settings.skipped.as_ref() != Some(&r.version) => {
                Some(r.version)
            }
            State::Downloading { release, .. } | State::Ready { release, .. } => {
                Some(release.version)
            }
            _ => None,
        }
    }

    /// Une mise à jour est à proposer (point sur le menu).
    pub fn update_offered(&self) -> bool {
        self.offered_version().is_some()
    }

    /// Pastille « Mise à jour » de la barre du haut (à droite, avant la carte son).
    pub fn update_pill(&mut self, ui: &mut egui::Ui) {
        let Some(v) = self.offered_version() else {
            return;
        };
        let label = match self.updater.state() {
            State::Downloading { file, size, .. } => {
                format!("{:.0} %", update::progress(&file, size) * 100.0)
            }
            _ => format!("Version {v}"),
        };
        let font = t::semibold(11.0);
        let gal = ui.painter().layout_no_wrap(label, font, t::ACCENT);
        let (r, resp) = ui.allocate_exact_size(vec2(gal.size().x + 32.0, 24.0), Sense::click());
        let p = ui.painter();
        let bg = if resp.hovered() {
            t::mix(t::ACCENT_DIM, t::ACCENT, 0.18)
        } else {
            t::ACCENT_DIM
        };
        p.rect_filled(r, CornerRadius::same(12), bg);
        p.rect_stroke(
            r,
            CornerRadius::same(12),
            Stroke::new(1.0, t::mix(t::ACCENT_DIM, t::ACCENT, 0.55)),
            StrokeKind::Inside,
        );
        w::paint_icon(
            p,
            Icon::Download,
            pos2(r.left() + 13.0, r.center().y),
            5.0,
            t::ACCENT,
        );
        p.galley(
            pos2(r.left() + 23.0, r.center().y - gal.size().y / 2.0),
            gal,
            t::ACCENT,
        );
        if resp
            .on_hover_text(format!(
                "TracksPlus {v} est disponible (vous avez la {})",
                update::CURRENT
            ))
            .clicked()
        {
            self.open_update();
        }
        ui.add_space(6.0);
    }

    /// Point orange sur le bouton du menu quand une mise à jour attend.
    pub fn update_menu_dot(&self, ui: &egui::Ui, menu: egui::Rect) {
        if self.update_offered() {
            ui.painter()
                .circle_filled(menu.right_top() + vec2(-6.0, 6.0), 3.5, t::ACCENT);
        }
    }

    /// Entrées du menu principal.
    pub fn update_menu(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            w::app_icon(ui, 14.0);
            ui.label(
                egui::RichText::new(format!("TRACKSPLUS {}", update::CURRENT))
                    .font(t::semibold(10.5))
                    .color(t::TEXT_3),
            );
        });
        if let Some(v) = self.offered_version() {
            if ui
                .button(egui::RichText::new(format!("Mettre à jour vers la {v}…")).color(t::ACCENT))
                .clicked()
            {
                self.open_update();
                ui.close();
            }
        } else if ui
            .add_enabled(
                !self.updater.busy(),
                egui::Button::new("Rechercher des mises à jour"),
            )
            .clicked()
        {
            self.update_settings.skipped = None;
            self.updater.check(ui.ctx());
            self.open_update();
            ui.close();
        }
    }

    pub fn open_update(&mut self) {
        self.close_dialogs();
        self.update_dlg = true;
    }

    pub fn update_dialog(&mut self, ctx: &egui::Context) {
        if !self.update_dlg {
            return;
        }
        let state = self.updater.state();
        let mut close = false;
        // Lecture, prise ou pads en cours : pas d'installation (elle fermerait l'application).
        let recording = !self.is_stopped() || self.pads_status().0 > 0;
        let installed = update::installed();
        egui::Modal::new(egui::Id::unique("update_dlg"))
            .frame(t::window_frame())
            .show(ctx, |ui| {
                ui.set_width(460.0);
                close = Self::dialog_header(ui, "Mises à jour");
                match &state {
                    State::Idle | State::Checking => {
                        ui.horizontal(|ui| {
                            ui.add(egui::Spinner::new().size(16.0).color(t::ACCENT));
                            ui.label(
                                egui::RichText::new("Recherche de la dernière version…")
                                    .color(t::TEXT_2),
                            );
                        });
                        ui.ctx().request_repaint();
                    }
                    State::UpToDate => {
                        ui.horizontal(|ui| {
                            w::app_icon(ui, 40.0);
                            ui.add_space(6.0);
                            ui.vertical(|ui| {
                                ui.label(
                                    egui::RichText::new("TracksPlus est à jour")
                                        .font(t::semibold(16.0))
                                        .color(t::TEXT),
                                );
                                ui.add_space(2.0);
                                ui.label(
                                    egui::RichText::new(format!(
                                        "Vous avez la dernière version ({}).",
                                        update::CURRENT
                                    ))
                                    .color(t::TEXT_2),
                                );
                            });
                        });
                    }
                    State::Failed(e) => {
                        ui.label(
                            egui::RichText::new("Impossible de vérifier les mises à jour")
                                .font(t::semibold(15.0))
                                .color(t::TEXT),
                        );
                        ui.add_space(4.0);
                        let mut msg = e.clone();
                        if let Some(f) = msg.get(..1) {
                            msg = f.to_uppercase() + &msg[1..];
                        }
                        ui.label(egui::RichText::new(format!("{msg}.")).color(t::TEXT_2));
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            if w::secondary_button(ui, "Réessayer").clicked() {
                                self.updater.check(ui.ctx());
                            }
                            if w::secondary_button(ui, "Voir les versions sur GitHub").clicked() {
                                update::open_url(&update::releases_page());
                            }
                        });
                    }
                    State::Available(r) => {
                        Self::release_header(ui, &r.version);
                        Self::release_notes(ui, &r.notes);
                        ui.add_space(14.0);
                        ui.horizontal(|ui| {
                            let can_install = installed && r.installer.is_some();
                            if can_install {
                                let resp = w::primary_button(
                                    ui,
                                    Some(Icon::Download),
                                    "Installer et redémarrer",
                                    !recording,
                                );
                                if recording {
                                    resp.on_hover_text("Arrêtez la lecture, la prise et les pads d'abord");
                                } else if resp.clicked() {
                                    self.update_install = true;
                                    self.updater.download(r.clone(), ui.ctx());
                                }
                            } else if w::primary_button(
                                ui,
                                Some(Icon::Download),
                                "Télécharger",
                                true,
                            )
                            .on_hover_text("Ouvre la page de la version (installeur et version portable)")
                            .clicked()
                            {
                                update::open_url(&r.page);
                            }
                            if w::secondary_button(ui, "Plus tard").clicked() {
                                close = true;
                            }
                            if w::secondary_button(ui, "Ignorer cette version").clicked() {
                                self.update_settings.skipped = Some(r.version.clone());
                                self.update_settings.save();
                                close = true;
                            }
                        });
                    }
                    State::Downloading {
                        release,
                        file,
                        size,
                    } => {
                        Self::release_header(ui, &release.version);
                        ui.add_space(6.0);
                        let p = update::progress(file, *size);
                        ui.add(
                            egui::ProgressBar::new(p)
                                .desired_width(460.0)
                                .desired_height(8.0)
                                .fill(t::ACCENT),
                        );
                        ui.add_space(6.0);
                        ui.label(
                            egui::RichText::new(format!(
                                "Téléchargement… {:.0} %  ·  {:.1} / {:.1} Mo",
                                p * 100.0,
                                *size as f64 * p as f64 / 1e6,
                                *size as f64 / 1e6
                            ))
                            .color(t::TEXT_2),
                        );
                        ui.ctx().request_repaint();
                    }
                    State::Ready { release, .. } => {
                        Self::release_header(ui, &release.version);
                        if self.update_install {
                            ui.label(
                                egui::RichText::new(
                                    "Installation : TracksPlus va se fermer puis se relancer.",
                                )
                                .color(t::TEXT_2),
                            );
                        } else {
                            ui.label(
                                egui::RichText::new("Mise à jour téléchargée et vérifiée.")
                                    .color(t::TEXT_2),
                            );
                            ui.add_space(10.0);
                            let busy = !self.is_stopped() || self.pads_status().0 > 0;
                            let resp = w::primary_button(
                                ui,
                                Some(Icon::Download),
                                "Installer et redémarrer",
                                !busy,
                            );
                            if busy {
                                resp.on_hover_text(
                                    "Arrêtez la lecture, la prise et les pads d'abord",
                                );
                            } else if resp.clicked() {
                                self.update_install = true;
                            }
                        }
                    }
                }
                ui.add_space(14.0);
                let mut auto = self.update_settings.auto_check;
                if ui
                    .checkbox(&mut auto, "Rechercher les mises à jour au lancement")
                    .changed()
                {
                    self.update_settings.auto_check = auto;
                    self.update_settings.save();
                }
                if !installed && matches!(state, State::Available(_)) {
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(
                            "Version portable : remplacez TracksPlus.exe par celui de la nouvelle version (réglages et sessions sont conservés).",
                        )
                        .font(t::regular(11.0))
                        .color(t::TEXT_3),
                    );
                }
            });
        if close {
            self.update_dlg = false;
        }
    }

    fn release_header(ui: &mut egui::Ui, version: &str) {
        ui.horizontal(|ui| {
            w::app_icon(ui, 40.0);
            ui.add_space(6.0);
            ui.vertical(|ui| {
                ui.label(
                    egui::RichText::new(format!("TracksPlus {version} est disponible"))
                        .font(t::semibold(16.0))
                        .color(t::TEXT),
                );
                ui.add_space(2.0);
                ui.label(
                    egui::RichText::new(format!("Vous avez la version {}", update::CURRENT))
                        .font(t::regular(11.5))
                        .color(t::TEXT_3),
                );
            });
        });
    }

    fn release_notes(ui: &mut egui::Ui, notes: &str) {
        let lines = update::plain_notes(notes);
        if lines.is_empty() {
            return;
        }
        ui.add_space(10.0);
        egui::Frame::new()
            .fill(t::SURFACE_2)
            .corner_radius(t::R)
            .inner_margin(12)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                egui::ScrollArea::vertical()
                    .max_height(220.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        for (heading, text) in lines {
                            let rt = if heading {
                                egui::RichText::new(text)
                                    .font(t::semibold(12.0))
                                    .color(t::TEXT)
                            } else {
                                egui::RichText::new(text)
                                    .font(t::regular(12.0))
                                    .color(Color32::from_gray(0xc8))
                            };
                            ui.add(egui::Label::new(rt).wrap());
                        }
                    });
            });
    }
}
