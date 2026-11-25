//! Écran d'accueil, au lancement : reprendre la dernière session, en créer une, en ouvrir
//! une, ou choisir dans la liste des sessions.

use crate::app::App;
use crate::sessions::{self, SessionInfo};
use crate::ui::theme as t;
use crate::ui::widgets::{self as w, Icon};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, Rect, Sense, Stroke, StrokeKind, pos2, vec2,
};
use std::path::PathBuf;

/// Taille du panneau d'accueil.
const WELCOME_SIZE: egui::Vec2 = egui::vec2(820.0, 500.0);

/// Hauteur d'une ligne de la liste des sessions (espacement compris).
const ROW_H: f32 = 52.0;

/// État de l'écran d'accueil ouvert.
pub struct Welcome {
    pub sessions: Vec<SessionInfo>,
    pub filter: String,
    /// Dernière session utilisée (proposée en grand).
    pub last: Option<SessionInfo>,
    /// Liste des sessions en cours de lecture (thread d'arrière-plan).
    loading: Option<crossbeam_channel::Receiver<Vec<SessionInfo>>>,
}

impl Welcome {
    /// L'écran s'affiche tout de suite : la liste (qui lit chaque session) se charge en
    /// arrière-plan, la fenêtre étant redessinée dès qu'elle est prête.
    pub fn with_ctx(ctx: &egui::Context) -> Self {
        Self::start(Some(ctx.clone()))
    }

    fn start(ctx: Option<egui::Context>) -> Self {
        // Seule la dernière session est lue tout de suite (carte « Reprendre »).
        let last = crate::recovery::last_session().and_then(|p| sessions::info(&p));
        let (tx, rx) = crossbeam_channel::bounded(1);
        let spawned = std::thread::Builder::new()
            .name("tracksplus-sessions".into())
            .spawn(move || {
                let _ = tx.send(sessions::list());
                if let Some(c) = ctx {
                    c.request_repaint();
                }
            });
        let mut wl = Self {
            sessions: Vec::new(),
            filter: String::new(),
            last,
            loading: Some(rx),
        };
        if spawned.is_err() {
            wl.loading = None;
            wl.set_sessions(sessions::list());
        }
        wl
    }

    fn set_sessions(&mut self, list: Vec<SessionInfo>) {
        if self.last.is_none() {
            self.last = list.first().cloned();
        }
        self.sessions = list;
    }

    /// Reçoit la liste si elle est prête. `true` tant qu'elle se charge.
    fn poll(&mut self, ctx: &egui::Context) -> bool {
        let Some(rx) = &self.loading else {
            return false;
        };
        match rx.try_recv() {
            Ok(list) => {
                self.loading = None;
                self.set_sessions(list);
                false
            }
            Err(crossbeam_channel::TryRecvError::Empty) => {
                // Sans contexte fourni au thread : on revient voir régulièrement.
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
                true
            }
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                self.loading = None;
                false
            }
        }
    }
}

enum Action {
    Open(PathBuf),
    New,
    Browse,
}

impl App {
    /// Écran d'accueil du lancement : page entière, aucune session affichée derrière tant
    /// que l'utilisateur n'a pas choisi.
    pub fn welcome_page(&mut self, ui: &mut egui::Ui) {
        let Some(mut wl) = self.welcome.take() else {
            return;
        };
        wl.poll(ui.ctx());
        let mut action = None;
        let area = ui.max_rect();
        let panel = Rect::from_center_size(area.center(), WELCOME_SIZE);
        let p = ui.painter();
        p.add(egui::Shape::Rect(egui::epaint::RectShape::filled(
            panel.translate(vec2(0.0, 8.0)).expand(6.0),
            CornerRadius::same(10),
            Color32::from_black_alpha(70),
        )));
        p.rect_filled(panel, CornerRadius::same(t::R_PANEL), t::SURFACE);
        p.rect_stroke(
            panel,
            CornerRadius::same(t::R_PANEL),
            Stroke::new(1.0, t::LINE),
            StrokeKind::Outside,
        );
        let mut pui = ui.new_child(egui::UiBuilder::new().max_rect(panel));
        Self::welcome_body(&mut pui, panel, &mut wl, &mut action);
        self.welcome_action(wl, action);
    }

    fn welcome_body(ui: &mut egui::Ui, full: Rect, wl: &mut Welcome, action: &mut Option<Action>) {
        let left = Rect::from_min_size(full.min, vec2(290.0, full.height()));
        let right = Rect::from_min_max(pos2(left.right(), full.top()), full.max);
        ui.painter().rect_filled(
            left,
            CornerRadius {
                nw: t::R_PANEL,
                sw: t::R_PANEL,
                ne: 0,
                se: 0,
            },
            t::SURFACE_2,
        );
        w::vline(
            ui.painter(),
            left.right(),
            full.top(),
            full.bottom(),
            t::LINE,
        );
        let mut lui = ui.new_child(egui::UiBuilder::new().max_rect(left.shrink(24.0)));
        Self::welcome_left(&mut lui, wl, action);
        let mut rui =
            ui.new_child(egui::UiBuilder::new().max_rect(right.shrink2(vec2(22.0, 20.0))));
        Self::welcome_right(&mut rui, wl, action);
    }

    fn welcome_action(&mut self, wl: Welcome, action: Option<Action>) {
        match action {
            None => self.welcome = Some(wl),
            Some(Action::Open(path)) => {
                if self.save_before_leaving() {
                    self.load_session(&path);
                }
            }
            Some(Action::New) => self.open_new_session(),
            // Explorateur intégré ; au lancement, l'accueil revient si on l'annule.
            Some(Action::Browse) => self.open_session(),
        }
    }

    fn welcome_left(ui: &mut egui::Ui, wl: &mut Welcome, action: &mut Option<Action>) {
        ui.spacing_mut().item_spacing.y = 8.0;
        ui.horizontal(|ui| {
            w::app_icon(ui, 34.0);
            ui.add_space(4.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.label(
                    egui::RichText::new("TracksPlus")
                        .font(t::semibold(18.0))
                        .color(t::TEXT),
                );
                ui.label(
                    egui::RichText::new(format!("Version {}", crate::update::CURRENT))
                        .font(t::regular(11.0))
                        .color(t::TEXT_3),
                );
            });
        });
        ui.add_space(22.0);

        // Reprendre : la dernière session, en grand.
        if let Some(last) = &wl.last {
            ui.label(
                egui::RichText::new("REPRENDRE")
                    .font(t::caps(10.5))
                    .color(t::TEXT_3),
            );
            let (r, resp) =
                ui.allocate_exact_size(vec2(ui.available_width(), 74.0), Sense::click());
            let p = ui.painter();
            let hov = resp.hovered();
            p.rect_filled(
                r,
                CornerRadius::same(t::R_PANEL),
                if hov {
                    t::mix(t::ACCENT_DIM, t::ACCENT, 0.12)
                } else {
                    t::ACCENT_DIM
                },
            );
            p.rect_stroke(
                r,
                CornerRadius::same(t::R_PANEL),
                Stroke::new(
                    1.0,
                    t::mix(t::ACCENT_DIM, t::ACCENT, if hov { 0.8 } else { 0.45 }),
                ),
                StrokeKind::Inside,
            );
            let c = pos2(r.left() + 24.0, r.center().y);
            p.circle_filled(c, 13.0, t::ACCENT);
            w::paint_icon(p, Icon::Play, c + vec2(1.5, 0.0), 6.0, Color32::BLACK);
            let tx = r.left() + 48.0;
            let name = w::fit_text(ui, &last.name, &t::semibold(13.5), r.right() - tx - 10.0);
            p.text(
                pos2(tx, r.top() + 18.0),
                Align2::LEFT_CENTER,
                name,
                t::semibold(13.5),
                t::TEXT,
            );
            p.text(
                pos2(tx, r.top() + 38.0),
                Align2::LEFT_CENTER,
                &last.date,
                t::regular(11.5),
                t::TEXT,
            );
            p.text(
                pos2(tx, r.top() + 55.0),
                Align2::LEFT_CENTER,
                &last.summary,
                t::regular(11.5),
                t::TEXT_2,
            );
            if resp
                .on_hover_ui(|ui| {
                    ui.label(last.path.display().to_string());
                })
                .clicked()
            {
                *action = Some(Action::Open(last.path.clone()));
            }
            ui.add_space(14.0);
        }

        ui.label(
            egui::RichText::new("COMMENCER")
                .font(t::caps(10.5))
                .color(t::TEXT_3),
        );
        if big_button(ui, Icon::Plus, "Nouvelle session…", "Dossier, nom, pistes") {
            *action = Some(Action::New);
        }
        if big_button(
            ui,
            Icon::Folder,
            "Ouvrir une session…",
            "Fichier .tracksplus ailleurs",
        ) {
            *action = Some(Action::Browse);
        }
    }

    fn welcome_right(ui: &mut egui::Ui, wl: &mut Welcome, action: &mut Option<Action>) {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("SESSIONS")
                    .font(t::caps(10.5))
                    .color(t::TEXT_3),
            );
            if wl.loading.is_none() {
                ui.label(
                    egui::RichText::new(format!("{}", wl.sessions.len()))
                        .font(t::regular(10.5))
                        .color(t::TEXT_3),
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut wl.filter)
                        .hint_text("Rechercher…")
                        .desired_width(200.0),
                );
            });
        });
        ui.add_space(10.0);
        if wl.loading.is_some() {
            ui.add_space(60.0);
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new("Chargement des sessions…").color(t::TEXT_3));
            });
            return;
        }
        let needle = wl.filter.trim().to_lowercase();
        let shown: Vec<&SessionInfo> = wl
            .sessions
            .iter()
            .filter(|s| needle.is_empty() || s.lower.contains(&needle))
            .collect();
        if shown.is_empty() {
            ui.add_space(60.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new(if wl.sessions.is_empty() {
                        "Aucune session pour l'instant : créez-en une à gauche."
                    } else {
                        "Aucune session ne correspond à la recherche."
                    })
                    .color(t::TEXT_3),
                );
            });
            return;
        }
        // Lignes de hauteur fixe : seules les lignes visibles sont dessinées.
        ui.spacing_mut().item_spacing.y = 0.0;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_rows(ui, ROW_H, shown.len(), |ui, range| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for s in &shown[range] {
                    let (cell, resp) =
                        ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
                    let r = Rect::from_min_size(cell.min, vec2(cell.width(), ROW_H - 2.0));
                    let p = ui.painter();
                    if resp.hovered() {
                        p.rect_filled(r, CornerRadius::same(t::R), t::SURFACE_3);
                    }
                    let empty = s.takes == 0 && !s.interrupted;
                    let name_col = if empty { t::TEXT_2 } else { t::TEXT };
                    let date = &s.date;
                    let date_w = p
                        .layout_no_wrap(date.clone(), t::regular(11.5), t::TEXT_3)
                        .size()
                        .x;
                    let name =
                        w::fit_text(ui, &s.name, &t::semibold(13.0), r.width() - date_w - 40.0);
                    p.text(
                        pos2(r.left() + 12.0, r.top() + 17.0),
                        Align2::LEFT_CENTER,
                        name,
                        t::semibold(13.0),
                        name_col,
                    );
                    p.text(
                        pos2(r.right() - 12.0, r.top() + 17.0),
                        Align2::RIGHT_CENTER,
                        date,
                        t::regular(11.5),
                        t::TEXT_3,
                    );
                    p.text(
                        pos2(r.left() + 12.0, r.top() + 35.0),
                        Align2::LEFT_CENTER,
                        &s.summary,
                        t::regular(11.5),
                        // Prise interrompue : à remarquer (ses prises attendent l'ouverture).
                        if s.interrupted { t::ORANGE } else { t::TEXT_3 },
                    );
                    // Bulle construite seulement au survol.
                    if resp
                        .on_hover_ui(|ui| {
                            ui.label(format!("{}\nClic : ouvrir", s.path.display()));
                        })
                        .clicked()
                    {
                        *action = Some(Action::Open(s.path.clone()));
                    }
                }
            });
    }
}

/// Bouton large de la colonne de gauche : icône, titre, sous-titre.
fn big_button(ui: &mut egui::Ui, icon: Icon, title: &str, sub: &str) -> bool {
    let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 46.0), Sense::click());
    let p = ui.painter();
    p.rect_filled(
        r,
        CornerRadius::same(t::R_PANEL),
        if resp.hovered() {
            t::SURFACE_4
        } else {
            t::SURFACE_3
        },
    );
    w::paint_icon(p, icon, pos2(r.left() + 20.0, r.center().y), 6.0, t::TEXT);
    p.text(
        pos2(r.left() + 40.0, r.top() + 16.0),
        Align2::LEFT_CENTER,
        title,
        t::semibold(12.5),
        t::TEXT,
    );
    p.text(
        pos2(r.left() + 40.0, r.top() + 32.0),
        Align2::LEFT_CENTER,
        sub,
        t::regular(11.0),
        t::TEXT_3,
    );
    resp.clicked()
}
