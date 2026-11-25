//! Explorateur intégré, au design de l'application : choisir les sons d'un pad, ou une
//! session à ouvrir, sans fenêtre système (raccourcis vers les dossiers usuels, chemin
//! cliquable, recherche, sélection multiple pour les sons).

use crate::app::App;
use crate::project::{PROJECT_EXT, default_sessions_root};
use crate::soundboard::{SOUNDS_DIR, is_audio_file};
use crate::ui::theme as t;
use crate::ui::widgets::{self as w, BtnStyle, Icon};
use eframe::egui::{self, Align2, CornerRadius, Key, Rect, Sense, pos2, vec2};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const SIZE: egui::Vec2 = egui::vec2(880.0, 560.0);
const ROW_H: f32 = 28.0;

struct Entry {
    path: PathBuf,
    name: String,
    is_dir: bool,
    size: u64,
    modified: Option<SystemTime>,
    /// Nom en minuscules (recherche), calculé une fois à la lecture du dossier.
    lower: String,
    /// Date affichée, calculée une fois.
    date: String,
}

impl Entry {
    fn new(
        path: PathBuf,
        name: String,
        is_dir: bool,
        size: u64,
        modified: Option<SystemTime>,
    ) -> Self {
        Self {
            lower: name.to_lowercase(),
            date: modified.map(crate::sessions::fmt_date).unwrap_or_default(),
            path,
            name,
            is_dir,
            size,
            modified,
        }
    }
}

/// Ce que l'on choisit.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Sons d'un pad : le premier son va dans le pad `slot`, les suivants dans les pads
    /// vides d'après.
    Sounds { slot: u16, multiple: bool },
    /// Session à ouvrir (fichier `.tracksplus`).
    Session,
}

impl Kind {
    fn accepts(self, p: &Path) -> bool {
        match self {
            Kind::Sounds { .. } => is_audio_file(p),
            Kind::Session => p.extension().is_some_and(|e| e == PROJECT_EXT),
        }
    }
}

/// Explorateur ouvert.
pub struct FileBrowser {
    dir: PathBuf,
    entries: Vec<Entry>,
    /// Indices (dans `entries`) des sons sélectionnés.
    selected: Vec<usize>,
    anchor: Option<usize>,
    filter: String,
    error: Option<String>,
    multiple: bool,
    kind: Kind,
    /// Remonter la liste en haut (changement de dossier).
    scroll_top: bool,
    /// Dernier changement de dossier : les clics qui suivent de près (fin d'un double-clic)
    /// ne doivent pas agir sur ce qui se trouve sous la souris dans le nouveau dossier.
    nav_at: std::time::Instant,
    /// Raccourcis de gauche, calculés à l'ouverture (test des lecteurs A: à Z:).
    places: Vec<Place>,
}

/// Raccourci de la colonne de gauche.
struct Place {
    label: String,
    path: PathBuf,
    icon: Icon,
}

impl FileBrowser {
    pub fn new(kind: Kind, session_dir: &Path) -> Self {
        let start = match kind {
            Kind::Sounds { .. } => crate::sessions::StartupPrefs::load()
                .sound_dir
                .filter(|d| d.is_dir())
                .or_else(|| {
                    let s = session_dir.join(SOUNDS_DIR);
                    s.is_dir().then_some(s)
                }),
            Kind::Session => Some(default_sessions_root()).filter(|d| d.is_dir()),
        }
        .or_else(dirs::audio_dir)
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."));
        let multiple = matches!(kind, Kind::Sounds { multiple: true, .. });
        let mut b = Self {
            dir: PathBuf::new(),
            entries: Vec::new(),
            selected: Vec::new(),
            anchor: None,
            filter: String::new(),
            error: None,
            multiple,
            kind,
            scroll_top: true,
            nav_at: std::time::Instant::now(),
            places: places(session_dir, kind),
        };
        b.open_dir(start);
        b
    }

    fn open_dir(&mut self, dir: PathBuf) {
        self.selected.clear();
        self.anchor = None;
        self.filter.clear();
        self.scroll_top = true;
        self.nav_at = std::time::Instant::now();
        match read_entries(&dir, self.kind) {
            Ok(e) => {
                self.entries = e;
                self.error = None;
            }
            Err(e) => {
                self.entries.clear();
                self.error = Some(format!("Dossier illisible : {e}"));
            }
        }
        self.dir = dir;
    }

    fn chosen(&self) -> Vec<PathBuf> {
        let mut idx = self.selected.clone();
        idx.sort();
        idx.into_iter()
            .filter_map(|i| self.entries.get(i))
            .filter(|e| !e.is_dir)
            .map(|e| e.path.clone())
            .collect()
    }

    /// Clic sur un son : sélection simple, Ctrl = ajouter / retirer, Maj = plage.
    fn click(&mut self, i: usize, ctrl: bool, shift: bool, visible: &[usize]) {
        if self.multiple
            && shift
            && let Some(a) = self.anchor
        {
            let (pa, pi) = (
                visible.iter().position(|&x| x == a),
                visible.iter().position(|&x| x == i),
            );
            if let (Some(pa), Some(pi)) = (pa, pi) {
                let (lo, hi) = (pa.min(pi), pa.max(pi));
                self.selected = visible[lo..=hi]
                    .iter()
                    .copied()
                    .filter(|&k| !self.entries[k].is_dir)
                    .collect();
                return;
            }
        }
        if self.multiple && ctrl {
            if let Some(p) = self.selected.iter().position(|&x| x == i) {
                self.selected.remove(p);
            } else {
                self.selected.push(i);
            }
        } else {
            self.selected = vec![i];
        }
        self.anchor = Some(i);
    }
}

fn read_entries(dir: &Path, kind: Kind) -> std::io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir)?.flatten() {
        let path = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || name.starts_with('$') {
            continue;
        }
        let Ok(meta) = e.metadata() else {
            continue;
        };
        let is_dir = meta.is_dir();
        if !is_dir && !kind.accepts(&path) {
            continue;
        }
        if is_dir && is_hidden(&meta) {
            continue;
        }
        // Dossier de session : affiché comme la session elle-même (un clic la choisit).
        if is_dir
            && kind == Kind::Session
            && let Some(f) = session_file(&path, &name)
        {
            let modified = std::fs::metadata(&f).and_then(|m| m.modified()).ok();
            out.push(Entry::new(f, name, false, 0, modified));
            continue;
        }
        out.push(Entry::new(
            path,
            name,
            is_dir,
            meta.len(),
            meta.modified().ok(),
        ));
    }
    out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.lower.cmp(&b.lower)));
    Ok(out)
}

/// Fichier de session d'un dossier : `<dossier>.tracksplus`, sinon le premier trouvé.
fn session_file(dir: &Path, name: &str) -> Option<PathBuf> {
    let same = dir.join(format!("{name}.{PROJECT_EXT}"));
    if same.is_file() {
        return Some(same);
    }
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == PROJECT_EXT))
}

#[cfg(target_os = "windows")]
fn is_hidden(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    // FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM
    meta.file_attributes() & (0x2 | 0x4) != 0
}

#[cfg(not(target_os = "windows"))]
fn is_hidden(_: &std::fs::Metadata) -> bool {
    false
}

fn places(session_dir: &Path, kind: Kind) -> Vec<Place> {
    let mut v = Vec::new();
    let sessions = default_sessions_root();
    if kind == Kind::Session && sessions.is_dir() {
        v.push(Place {
            label: "Sessions".into(),
            path: sessions,
            icon: Icon::Record,
        });
    }
    let sounds = session_dir.join(SOUNDS_DIR);
    if matches!(kind, Kind::Sounds { .. }) && sounds.is_dir() {
        v.push(Place {
            label: "Sons de la session".into(),
            path: sounds,
            icon: Icon::Speaker,
        });
    }
    for (label, p) in [
        ("Musique", dirs::audio_dir()),
        ("Bureau", dirs::desktop_dir()),
        ("Documents", dirs::document_dir()),
        ("Téléchargements", dirs::download_dir()),
    ] {
        if let Some(p) = p.filter(|p| p.is_dir()) {
            v.push(Place {
                label: label.into(),
                path: p,
                icon: Icon::Folder,
            });
        }
    }
    // Disques présents, demandés au système d'un coup (un lecteur réseau déconnecté n'est
    // pas interrogé : pas de blocage à l'ouverture).
    #[cfg(target_os = "windows")]
    {
        let mask = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
        for (k, l) in (b'A'..=b'Z').enumerate() {
            if mask & (1 << k) != 0 {
                v.push(Place {
                    label: format!("Disque {}:", l as char),
                    path: PathBuf::from(format!("{}:\\", l as char)),
                    icon: Icon::Folder,
                });
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    v.push(Place {
        label: "Racine".into(),
        path: PathBuf::from("/"),
        icon: Icon::Folder,
    });
    v
}

fn fmt_size(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1} Mo", n as f64 / 1e6)
    } else {
        format!("{} Ko", n.div_ceil(1000).max(1))
    }
}

enum Outcome {
    Cancel,
    Pick(Vec<PathBuf>),
}

impl App {
    /// Ouvre l'explorateur de sons pour le pad `slot`.
    pub fn open_browser(&mut self, slot: u16, multiple: bool) {
        self.browser = Some(FileBrowser::new(
            Kind::Sounds { slot, multiple },
            &self.project.dir,
        ));
    }

    /// Ouvre l'explorateur pour choisir une session à ouvrir.
    pub fn open_session_browser(&mut self) {
        self.browser = Some(FileBrowser::new(Kind::Session, &self.project.dir));
    }

    pub fn browser_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut b) = self.browser.take() else {
            return;
        };
        let session = b.kind == Kind::Session;
        let places = std::mem::take(&mut b.places);
        let mut outcome = None;
        let mut go: Option<PathBuf> = None;
        let modal = egui::Modal::new(egui::Id::unique("sound_browser"))
            .frame(t::window_frame())
            .show(ctx, |ui| {
                ui.set_width(SIZE.x);
                ui.set_height(SIZE.y);
                // ---- en-tête
                let title = if session {
                    "Ouvrir une session"
                } else if b.multiple {
                    "Choisir des sons"
                } else {
                    "Choisir un son"
                };
                if Self::dialog_header(ui, title) {
                    outcome = Some(Outcome::Cancel);
                }
                // ---- barre : dossier parent, chemin cliquable, recherche
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    let parent = b.dir.parent().map(Path::to_path_buf);
                    if w::icon_button(ui, Icon::Undo, 26.0, BtnStyle::Ghost, parent.is_some(), "Dossier parent")
                        .clicked()
                        && let Some(p) = parent
                    {
                        go = Some(p);
                    }
                    let search_w = 200.0;
                    let crumbs_w = ui.available_width() - search_w - 12.0;
                    ui.allocate_ui_with_layout(
                        vec2(crumbs_w, 26.0),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            ui.set_clip_rect(ui.max_rect());
                            ui.spacing_mut().item_spacing.x = 2.0;
                            let parts: Vec<PathBuf> = b.dir.ancestors().map(Path::to_path_buf).collect();
                            // Les derniers dossiers du chemin seulement, si c'est trop long.
                            let shown: Vec<&PathBuf> = parts.iter().rev().collect();
                            let skip = shown.len().saturating_sub(6);
                            if skip > 0 {
                                ui.label(egui::RichText::new("…").color(t::TEXT_3));
                            }
                            for (k, p) in shown.iter().enumerate().skip(skip) {
                                let name = p
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| p.to_string_lossy().trim_end_matches(['\\', '/']).to_string());
                                let last = k + 1 == shown.len();
                                let txt = egui::RichText::new(name).font(t::medium(12.0)).color(if last {
                                    t::TEXT
                                } else {
                                    t::TEXT_2
                                });
                                if ui.add(egui::Button::new(txt).frame(false)).clicked() && !last {
                                    go = Some((*p).clone());
                                }
                                if !last {
                                    ui.label(egui::RichText::new("›").color(t::TEXT_3));
                                }
                            }
                        },
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut b.filter)
                                .hint_text("Rechercher dans ce dossier…")
                                .desired_width(search_w),
                        );
                    });
                });
                ui.add_space(8.0);

                // ---- corps : raccourcis à gauche, contenu du dossier à droite
                let footer_h = 44.0;
                let body = Rect::from_min_max(
                    ui.cursor().min,
                    pos2(ui.max_rect().right(), ui.max_rect().bottom() - footer_h),
                );
                let side = Rect::from_min_size(body.min, vec2(190.0, body.height()));
                let list = Rect::from_min_max(pos2(side.right() + 10.0, body.top()), body.max);
                ui.painter().rect_filled(side, CornerRadius::same(t::R), t::SURFACE_2);
                ui.painter().rect_filled(list, CornerRadius::same(t::R), t::INSET);

                let mut sui = ui.new_child(egui::UiBuilder::new().max_rect(side.shrink(6.0)));
                egui::ScrollArea::vertical()
                    .id_salt("br_places")
                    .auto_shrink([false, false])
                    .show(&mut sui, |ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        for pl in &places {
                            let here = b.dir == pl.path;
                            let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 28.0), Sense::click());
                            let p = ui.painter();
                            if here {
                                p.rect_filled(r, CornerRadius::same(t::R), t::SURFACE_4);
                            } else if resp.hovered() {
                                p.rect_filled(r, CornerRadius::same(t::R), t::SURFACE_3);
                            }
                            let col = if here { t::ACCENT } else { t::TEXT_2 };
                            w::paint_icon(p, pl.icon, pos2(r.left() + 14.0, r.center().y), 5.0, col);
                            p.text(
                                pos2(r.left() + 28.0, r.center().y),
                                Align2::LEFT_CENTER,
                                &pl.label,
                                t::regular(12.0),
                                if here { t::TEXT } else { t::TEXT_2 },
                            );
                            if resp.on_hover_text(pl.path.display().to_string()).clicked() {
                                go = Some(pl.path.clone());
                            }
                        }
                    });

                let mut lui = ui.new_child(egui::UiBuilder::new().max_rect(list.shrink(4.0)));
                // En-têtes de colonnes.
                let cols_x = |r: Rect| (r.right() - 210.0, r.right() - 110.0);
                let (hr, _) = lui.allocate_exact_size(vec2(lui.available_width(), 22.0), Sense::hover());
                let (cx_size, cx_date) = cols_x(hr);
                let hp = lui.painter();
                hp.text(pos2(hr.left() + 34.0, hr.center().y), Align2::LEFT_CENTER, "NOM", t::caps(10.0), t::TEXT_3);
                if !session {
                    hp.text(pos2(cx_size + 80.0, hr.center().y), Align2::RIGHT_CENTER, "TAILLE", t::caps(10.0), t::TEXT_3);
                }
                hp.text(pos2(cx_date + 10.0, hr.center().y), Align2::LEFT_CENTER, "MODIFIÉ", t::caps(10.0), t::TEXT_3);

                let needle = b.filter.trim().to_lowercase();
                let visible: Vec<usize> = (0..b.entries.len())
                    .filter(|&i| needle.is_empty() || b.entries[i].lower.contains(&needle))
                    .collect();
                let (ctrl, shift) = ui.input(|i| (i.modifiers.command, i.modifiers.shift));
                let mut sa = egui::ScrollArea::vertical()
                    .id_salt(("br_list", b.dir.clone()))
                    .auto_shrink([false, false]);
                if b.scroll_top {
                    sa = sa.vertical_scroll_offset(0.0);
                    b.scroll_top = false;
                }
                sa.show_rows(&mut lui, ROW_H, visible.len().max(1), |ui, range| {
                    if visible.is_empty() {
                        ui.add_space(40.0);
                        ui.vertical_centered(|ui| {
                            let msg = b.error.clone().unwrap_or_else(|| {
                                if needle.is_empty() {
                                    if session {
                                        "Aucune session ni dossier ici".into()
                                    } else {
                                        "Aucun son ni dossier ici".into()
                                    }
                                } else {
                                    "Aucun résultat".into()
                                }
                            });
                            ui.label(egui::RichText::new(msg).color(t::TEXT_3));
                        });
                        return;
                    }
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for &i in &visible[range] {
                        let e = &b.entries[i];
                        let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
                        let sel = b.selected.contains(&i);
                        let p = ui.painter();
                        if sel {
                            p.rect_filled(r, CornerRadius::same(t::R), t::ACCENT_DIM);
                        } else if resp.hovered() {
                            p.rect_filled(r, CornerRadius::same(t::R), t::SURFACE_2);
                        }
                        let (icon, icol) = if e.is_dir {
                            (Icon::Folder, t::TEXT_2)
                        } else {
                            (
                                if session { Icon::Record } else { Icon::Speaker },
                                if sel { t::ACCENT } else { t::TEXT_3 },
                            )
                        };
                        w::paint_icon(p, icon, pos2(r.left() + 16.0, r.center().y), 5.0, icol);
                        let (cx_size, cx_date) = cols_x(r);
                        let name = w::fit_text(ui, &e.name, &t::regular(12.5), cx_size - r.left() - 44.0);
                        p.text(
                            pos2(r.left() + 34.0, r.center().y),
                            Align2::LEFT_CENTER,
                            name,
                            t::regular(12.5),
                            if e.is_dir || sel { t::TEXT } else { t::TEXT_2 },
                        );
                        if !e.is_dir && !session {
                            p.text(pos2(cx_size + 80.0, r.center().y), Align2::RIGHT_CENTER, fmt_size(e.size), t::regular(11.5), t::TEXT_3);
                        }
                        if e.modified.is_some() {
                            p.text(
                                pos2(cx_date + 10.0, r.center().y),
                                Align2::LEFT_CENTER,
                                &e.date,
                                t::regular(11.5),
                                t::TEXT_3,
                            );
                        }
                        let settled = b.nav_at.elapsed().as_millis() > 450;
                        if !settled {
                            // Second clic d'un double-clic sur un dossier : ignoré.
                        } else if resp.double_clicked() {
                            if e.is_dir {
                                go = Some(e.path.clone());
                            } else {
                                outcome = Some(Outcome::Pick(vec![e.path.clone()]));
                            }
                        } else if resp.clicked() {
                            if e.is_dir {
                                go = Some(e.path.clone());
                            } else {
                                b.click(i, ctrl, shift, &visible);
                            }
                        }
                    }
                });

                // ---- pied : nombre choisi, Annuler, Ajouter
                let chosen = b.chosen();
                let foot = Rect::from_min_max(pos2(ui.max_rect().left(), ui.max_rect().bottom() - footer_h + 12.0), ui.max_rect().max);
                let mut fui = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(foot)
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                );
                fui.label(
                    egui::RichText::new(match chosen.len() {
                        0 if session => "Clic : choisir · double-clic : ouvrir".to_string(),
                        0 if b.multiple => "Clic : choisir · Ctrl / Maj + clic : plusieurs sons · double-clic : ajouter".to_string(),
                        0 => "Clic : choisir · double-clic : ajouter".to_string(),
                        _ if session => "1 session sélectionnée".to_string(),
                        1 => "1 son sélectionné".to_string(),
                        n => format!("{n} sons sélectionnés"),
                    })
                    .font(t::regular(11.5))
                    .color(t::TEXT_3),
                );
                fui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let label = if session {
                        "Ouvrir"
                    } else if chosen.len() > 1 {
                        "Ajouter les sons"
                    } else {
                        "Ajouter"
                    };
                    if w::primary_button(ui, None, label, !chosen.is_empty()).clicked() {
                        outcome = Some(Outcome::Pick(chosen.clone()));
                    }
                    if w::secondary_button(ui, "Annuler").clicked() {
                        outcome = Some(Outcome::Cancel);
                    }
                });
                if !chosen.is_empty() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    outcome = Some(Outcome::Pick(chosen));
                }
            });
        if modal.should_close() && outcome.is_none() {
            outcome = Some(Outcome::Cancel);
        }
        b.places = places;
        if let Some(d) = go {
            b.open_dir(d);
        }
        match outcome {
            None => self.browser = Some(b),
            Some(Outcome::Cancel) => {}
            Some(Outcome::Pick(files)) => match b.kind {
                Kind::Sounds { slot, .. } => {
                    let mut prefs = crate::sessions::StartupPrefs::load();
                    prefs.sound_dir = Some(b.dir.clone());
                    prefs.save();
                    self.assign_files(slot, files);
                }
                Kind::Session => {
                    if let Some(f) = files.first() {
                        self.stop();
                        if self.save_before_leaving() {
                            self.load_session(f);
                        }
                    }
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Charge : dossier de 5 000 sons (+ 200 sous-dossiers) lu par l'explorateur.
    /// `cargo test --release -- --ignored --nocapture charge_explorateur`
    #[test]
    #[ignore]
    fn charge_explorateur_5000_sons() {
        let d = std::env::temp_dir().join(format!("tp-charge-br-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        for i in 0..5000 {
            std::fs::write(d.join(format!("son {i:05}.wav")), b"x").unwrap();
        }
        for i in 0..200 {
            std::fs::create_dir_all(d.join(format!("dossier {i:03}"))).unwrap();
        }
        let t = std::time::Instant::now();
        let e = read_entries(
            &d,
            Kind::Sounds {
                slot: 0,
                multiple: true,
            },
        )
        .unwrap();
        let read = t.elapsed();
        let t = std::time::Instant::now();
        let p = places(&d, Kind::Session);
        let pl = t.elapsed();
        println!(
            "explorateur : {} entrées lues en {read:?} · raccourcis ({} dont disques) en {pl:?}",
            e.len(),
            p.len()
        );
        assert_eq!(e.len(), 5200);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn lists_folders_then_sounds_only() {
        let d = std::env::temp_dir().join(format!("tp-browser-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("Batterie")).unwrap();
        for f in ["b.wav", "A.mp3", "notes.txt", ".cache.wav"] {
            std::fs::write(d.join(f), b"x").unwrap();
        }
        std::fs::write(d.join("Concert.tracksplus"), b"{}").unwrap();
        let sounds = Kind::Sounds {
            slot: 0,
            multiple: true,
        };
        let e = read_entries(&d, sounds).unwrap();
        let names: Vec<&str> = e.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Batterie", "A.mp3", "b.wav"]);
        assert!(e[0].is_dir);
        // Un dossier de session apparaît comme la session (son fichier).
        std::fs::create_dir_all(d.join("Live")).unwrap();
        std::fs::write(d.join("Live").join("Live.tracksplus"), b"{}").unwrap();
        let s = read_entries(&d, Kind::Session).unwrap();
        let names: Vec<&str> = s.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Batterie", "Concert.tracksplus", "Live"]);
        assert_eq!(s[2].path, d.join("Live").join("Live.tracksplus"));
        assert!(!s[2].is_dir);
        assert_eq!(fmt_size(1), "1 Ko");
        assert_eq!(fmt_size(2_500_000), "2.5 Mo");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn selection_with_ctrl_and_shift() {
        let mk = |n: &str, dir: bool| Entry::new(PathBuf::from(n), n.into(), dir, 0, None);
        let mut b = FileBrowser {
            dir: PathBuf::new(),
            entries: vec![
                mk("d", true),
                mk("a", false),
                mk("b", false),
                mk("c", false),
            ],
            selected: Vec::new(),
            anchor: None,
            filter: String::new(),
            error: None,
            multiple: true,
            kind: Kind::Sounds {
                slot: 0,
                multiple: true,
            },
            scroll_top: false,
            places: Vec::new(),
            nav_at: std::time::Instant::now(),
        };
        let vis = [0, 1, 2, 3];
        b.click(1, false, false, &vis);
        b.click(3, false, true, &vis);
        assert_eq!(
            b.chosen(),
            [PathBuf::from("a"), PathBuf::from("b"), PathBuf::from("c")]
        );
        b.click(2, true, false, &vis);
        assert_eq!(b.chosen(), [PathBuf::from("a"), PathBuf::from("c")]);
        b.multiple = false;
        b.click(2, true, false, &vis);
        assert_eq!(b.chosen(), [PathBuf::from("b")]);
    }
}
