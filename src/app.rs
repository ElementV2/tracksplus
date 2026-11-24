//! Interface : barre de transport, pistes, timeline, réglages audio et export.

use crate::audio::{self, AudioPrefs, DeviceInfo, Engine, RecTarget};
use crate::export::{BitDepth, ExportJob, ExportSettings};
use crate::player::Player;
use crate::project::{self, Clip, PROJECT_EXT, Project, RecFormat};
use crate::recorder::{RecSpec, Recorder};
use crate::render::{self, PEAK_BLOCK, PEAK_L1, PeakData, Peaks};
use crate::theme;
use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Key, Mesh, Pos2, Rect, Sense, Stroke, StrokeKind,
    UiBuilder, Vec2, pos2, vec2,
};
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

const HEADER_W: f32 = 270.0;
const RULER_H: f32 = 26.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Transport {
    Stopped,
    Playing { start: u64 },
    Recording { start: u64 },
}

struct LiveTake {
    track_id: u64,
    peaks: Peaks,
}

struct View {
    /// Échantillons par pixel.
    spp: f64,
    /// Échantillon au bord gauche de la timeline.
    scroll: f64,
}

struct AudioDialog {
    prefs: AudioPrefs,
    hosts: Vec<String>,
    ins: Vec<DeviceInfo>,
    outs: Vec<DeviceInfo>,
    listed_host: Option<String>,
    error: Option<String>,
}

impl AudioDialog {
    fn new(prefs: &AudioPrefs) -> Self {
        let mut d = Self {
            prefs: prefs.clone(),
            hosts: audio::host_names(),
            ins: Vec::new(),
            outs: Vec::new(),
            listed_host: None,
            error: None,
        };
        if d.prefs.host.is_none() {
            d.prefs.host = Some(audio::default_host_name());
        }
        d.refresh();
        d
    }

    fn refresh(&mut self) {
        let (ins, outs) = audio::list_devices(self.prefs.host.as_deref());
        self.ins = ins;
        self.outs = outs;
        self.listed_host = self.prefs.host.clone();
    }
}

struct NewSessionDialog {
    name: String,
    parent: PathBuf,
    keep_tracks: bool,
}

struct Toast {
    text: String,
    error: bool,
    until: f64,
}

pub struct App {
    project: Project,
    engine: Engine,
    prefs: AudioPrefs,
    transport: Transport,
    playhead: u64,
    player: Option<Player>,
    recorder: Option<Recorder>,
    live: Vec<LiveTake>,
    peaks: HashMap<u64, Peaks>,
    meters: Vec<f32>,
    clip_hold: Vec<f64>,
    view: View,
    audio_dlg: Option<AudioDialog>,
    export_dlg: Option<ExportSettings>,
    export_job: Option<ExportJob>,
    new_dlg: Option<NewSessionDialog>,
    toast: Option<Toast>,
    ctx_clip: Option<(u64, u64)>,
    disk_free: Option<u64>,
    disk_checked: f64,
    title: String,
    export_last: Option<ExportSettings>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::apply(&cc.egui_ctx);
        let prefs = AudioPrefs::load();
        let (engine, err) = match Engine::open(&prefs) {
            Ok(e) => (e, None),
            Err(e) => (Engine::offline(), Some(format!("{e:#}"))),
        };
        let mut project = Project::untitled(engine.status.sample_rate);
        project.add_track(None, Some(0));
        project.add_track(None, Some(1));
        project.dirty = false;
        let mut app = Self {
            project,
            engine,
            prefs,
            transport: Transport::Stopped,
            playhead: 0,
            player: None,
            recorder: None,
            live: Vec::new(),
            peaks: HashMap::new(),
            meters: vec![0.0; audio::MAX_INPUTS],
            clip_hold: vec![0.0; audio::MAX_INPUTS],
            view: View {
                spp: 2048.0,
                scroll: 0.0,
            },
            audio_dlg: None,
            export_dlg: None,
            export_job: None,
            new_dlg: None,
            toast: None,
            ctx_clip: None,
            disk_free: None,
            disk_checked: -100.0,
            title: String::new(),
            export_last: None,
        };
        if let Some(e) = err {
            app.audio_dlg = Some(AudioDialog::new(&app.prefs));
            if let Some(d) = &mut app.audio_dlg {
                d.error = Some(e);
            }
        }
        app
    }

    fn notify(&mut self, ctx: &egui::Context, text: impl Into<String>, error: bool) {
        let now = ctx.input(|i| i.time);
        self.toast = Some(Toast {
            text: text.into(),
            error,
            until: now + if error { 8.0 } else { 4.0 },
        });
    }

    fn sr(&self) -> u32 {
        self.engine.status.sample_rate.max(1)
    }

    // ---------------------------------------------------------------- transport

    fn is_stopped(&self) -> bool {
        self.transport == Transport::Stopped
    }

    fn play(&mut self, ctx: &egui::Context) {
        if !self.is_stopped() {
            return;
        }
        if !self.engine.has_output() {
            self.notify(
                ctx,
                "Aucune sortie audio : choisissez-en une dans Audio ▸ Réglages",
                true,
            );
            return;
        }
        let (player, cons) = Player::start(&self.project, self.playhead);
        self.engine.start_playback(cons);
        self.player = Some(player);
        self.transport = Transport::Playing {
            start: self.playhead,
        };
    }

    fn record(&mut self, ctx: &egui::Context) {
        if !self.is_stopped() {
            if matches!(self.transport, Transport::Playing { .. }) {
                self.stop(ctx);
            } else {
                return;
            }
        }
        if !self.engine.has_input() {
            self.notify(
                ctx,
                "Aucune entrée audio : choisissez votre carte dans Audio ▸ Réglages",
                true,
            );
            return;
        }
        let sr = self.engine.status.sample_rate;
        if self.project.has_audio() && self.project.sample_rate != sr {
            self.notify(
                ctx,
                format!(
                    "La session est en {} Hz mais la carte tourne en {} Hz. Changez la fréquence dans Audio ▸ Réglages.",
                    self.project.sample_rate, sr
                ),
                true,
            );
            return;
        }
        let in_ch = self.engine.status.in_channels;
        let armed: Vec<(u64, String, u16)> = self
            .project
            .tracks
            .iter()
            .filter(|t| t.armed)
            .filter_map(|t| t.input.map(|i| (t.id, t.name.clone(), i)))
            .filter(|(_, _, i)| *i < in_ch)
            .collect();
        if armed.is_empty() {
            self.notify(
                ctx,
                "Armez au moins une piste (bouton ● rouge) avec une entrée valide",
                true,
            );
            return;
        }
        self.project.sample_rate = sr;
        if let Err(e) = self.project.ensure_dirs() {
            self.notify(ctx, format!("{e:#}"), true);
            return;
        }

        let mut specs = Vec::new();
        let mut targets = Vec::new();
        let mut used = HashSet::new();
        self.live.clear();
        for (track_id, name, input) in armed {
            let mut file = self.project.new_take_file(&name);
            let mut k = 2;
            while used.contains(&file) {
                file = format!("{}_{k}.wav", file.trim_end_matches(".wav"));
                k += 1;
            }
            used.insert(file.clone());
            let (prod, cons) = rtrb::RingBuffer::new(sr as usize * 4);
            let peaks: Peaks = Arc::new(RwLock::new(PeakData::default()));
            specs.push(RecSpec {
                track_id,
                path: self.project.audio_dir().join(&file),
                file,
                peaks: peaks.clone(),
                cons,
            });
            targets.push(RecTarget {
                channel: input as usize,
                prod,
            });
            self.live.push(LiveTake { track_id, peaks });
        }
        let recorder = Recorder::start(specs, sr, self.project.rec_format);
        if let Err(e) = self.engine.start_record(targets) {
            let _ = recorder.finish();
            self.live.clear();
            self.notify(ctx, format!("{e:#}"), true);
            return;
        }
        // Écoute des pistes déjà enregistrées pendant la prise.
        if self.engine.has_output() && self.project.has_audio() {
            let (player, cons) = Player::start(&self.project, self.playhead);
            self.engine.start_playback(cons);
            self.player = Some(player);
        }
        self.recorder = Some(recorder);
        self.transport = Transport::Recording {
            start: self.playhead,
        };
    }

    fn stop(&mut self, ctx: &egui::Context) {
        match self.transport {
            Transport::Stopped => {}
            Transport::Playing { .. } => {
                self.engine.stop_playback();
                self.player = None;
            }
            Transport::Recording { start } => {
                self.engine.stop_record();
                self.engine.stop_playback();
                self.player = None;
                let results = self
                    .recorder
                    .take()
                    .map(Recorder::finish)
                    .unwrap_or_default();
                let mut errors = Vec::new();
                let mut max_len = 0;
                for r in results {
                    let live = self.live.iter().find(|l| l.track_id == r.track_id);
                    if let Some(e) = &r.error {
                        errors.push(e.clone());
                    }
                    if r.frames == 0 {
                        let _ = std::fs::remove_file(self.project.audio_dir().join(&r.file));
                        continue;
                    }
                    max_len = max_len.max(r.frames);
                    let id = self.project.alloc_id();
                    if let Some(l) = live {
                        self.peaks.insert(id, l.peaks.clone());
                    }
                    if let Some(t) = self.project.track_mut(r.track_id) {
                        t.clips.push(Clip {
                            id,
                            file: r.file,
                            start,
                            offset: 0,
                            len: r.frames,
                        });
                    }
                }
                self.live.clear();
                self.playhead = start + max_len;
                self.project.dirty = true;
                let save = self.project.save();
                let overruns = self.engine.shared.overruns.swap(0, Ordering::Relaxed);
                if !errors.is_empty() {
                    self.notify(
                        ctx,
                        format!("Erreur d'enregistrement : {}", errors.join(" ; ")),
                        true,
                    );
                } else if overruns > 0 {
                    self.notify(
                        ctx,
                        format!("Attention : {overruns} pertes de données (disque trop lent ?)"),
                        true,
                    );
                } else if let Err(e) = save {
                    self.notify(ctx, format!("Sauvegarde impossible : {e:#}"), true);
                } else {
                    self.notify(
                        ctx,
                        format!("Prise enregistrée ({})", fmt_time(max_len, self.sr())),
                        false,
                    );
                }
            }
        }
        self.transport = Transport::Stopped;
    }

    fn update_playhead(&mut self) {
        let sh = &self.engine.shared;
        match self.transport {
            Transport::Playing { start } => {
                self.playhead = start + sh.out_frames.load(Ordering::Relaxed)
            }
            Transport::Recording { start } => {
                self.playhead = start + sh.rec_frames.load(Ordering::Relaxed)
            }
            Transport::Stopped => {}
        }
    }

    // ---------------------------------------------------------------- session

    fn load_peaks(&mut self) {
        self.peaks.clear();
        for t in &self.project.tracks {
            for c in &t.clips {
                let p: Peaks = Arc::new(RwLock::new(PeakData::default()));
                self.peaks.insert(c.id, p.clone());
                let path = self.project.clip_path(c);
                std::thread::spawn(move || render::compute_peaks(&path, &p));
            }
        }
    }

    fn autosave(&mut self) {
        if self.project.dirty && self.project.has_audio() {
            let _ = self.project.save();
        }
    }

    fn open_session(&mut self, ctx: &egui::Context) {
        let Some(path) = rfd::FileDialog::new()
            .set_title("Ouvrir une session")
            .add_filter("Session TracksPlus", &[PROJECT_EXT])
            .set_directory(project::default_sessions_root())
            .pick_file()
        else {
            return;
        };
        self.stop(ctx);
        self.autosave();
        match Project::load(&path) {
            Ok(p) => {
                self.project = p;
                self.playhead = 0;
                self.view.scroll = 0.0;
                self.load_peaks();
                if self.project.has_audio()
                    && self.project.sample_rate != self.engine.status.sample_rate
                {
                    let msg = format!(
                        "Session en {} Hz, carte en {} Hz : réglez la carte sur {} Hz pour lire/enregistrer.",
                        self.project.sample_rate,
                        self.engine.status.sample_rate,
                        self.project.sample_rate
                    );
                    self.notify(ctx, msg, true);
                } else {
                    self.notify(
                        ctx,
                        format!("Session « {} » ouverte", self.project.name),
                        false,
                    );
                }
            }
            Err(e) => self.notify(ctx, format!("{e:#}"), true),
        }
    }

    fn save(&mut self, ctx: &egui::Context) {
        match self.project.save() {
            Ok(()) => self.notify(
                ctx,
                format!("Enregistré dans {}", self.project.dir.display()),
                false,
            ),
            Err(e) => self.notify(ctx, format!("{e:#}"), true),
        }
    }

    fn save_as(&mut self, ctx: &egui::Context) {
        let Some(parent) = rfd::FileDialog::new()
            .set_title("Choisir le dossier où créer la session")
            .set_directory(project::default_sessions_root())
            .pick_folder()
        else {
            return;
        };
        let name = self.project.name.clone();
        let dir = parent.join(project::sanitize(&name));
        match self.project.save_as(dir, name) {
            Ok(()) => self.notify(
                ctx,
                format!("Session enregistrée dans {}", self.project.dir.display()),
                false,
            ),
            Err(e) => self.notify(ctx, format!("{e:#}"), true),
        }
    }

    fn create_session(&mut self, ctx: &egui::Context, d: NewSessionDialog) {
        self.stop(ctx);
        self.autosave();
        let name = project::sanitize(&d.name);
        let dir = d.parent.join(&name);
        if dir.join(format!("{name}.{PROJECT_EXT}")).exists() {
            self.notify(
                ctx,
                format!("Une session « {name} » existe déjà à cet endroit"),
                true,
            );
            return;
        }
        let mut p = Project::new(name, dir, self.engine.status.sample_rate);
        p.rec_format = self.project.rec_format;
        if d.keep_tracks {
            for t in &self.project.tracks {
                let id = p.add_track(Some(t.name.clone()), t.input);
                if let Some(nt) = p.track_mut(id) {
                    nt.armed = t.armed;
                }
            }
        }
        if let Err(e) = p.save() {
            self.notify(ctx, format!("{e:#}"), true);
            return;
        }
        self.project = p;
        self.peaks.clear();
        self.playhead = 0;
        self.view.scroll = 0.0;
        self.notify(
            ctx,
            format!("Nouvelle session « {} »", self.project.name),
            false,
        );
    }

    fn add_track_per_input(&mut self) {
        let used: HashSet<u16> = self.project.tracks.iter().filter_map(|t| t.input).collect();
        for i in 0..self.engine.status.in_channels {
            if !used.contains(&i) {
                self.project
                    .add_track(Some(format!("Entrée {}", i + 1)), Some(i));
            }
        }
    }

    fn next_free_input(&self) -> Option<u16> {
        let used: HashSet<u16> = self.project.tracks.iter().filter_map(|t| t.input).collect();
        (0..self.engine.status.in_channels).find(|i| !used.contains(i))
    }

    fn apply_audio(&mut self, ctx: &egui::Context, prefs: AudioPrefs) -> Result<(), String> {
        self.stop(ctx);
        // Libère d'abord l'ancien pilote (ASIO n'en autorise qu'un à la fois).
        self.engine = Engine::offline();
        match Engine::open(&prefs) {
            Ok(e) => {
                self.engine = e;
                self.prefs = prefs;
                self.prefs.save();
                if !self.project.has_audio() {
                    self.project.sample_rate = self.engine.status.sample_rate;
                }
                Ok(())
            }
            Err(e) => {
                // On essaie de revenir à la configuration précédente.
                if let Ok(old) = Engine::open(&self.prefs) {
                    self.engine = old;
                }
                Err(format!("{e:#}"))
            }
        }
    }

    // ---------------------------------------------------------------- frame

    fn handle_keys(&mut self, ctx: &egui::Context) {
        let dialog_open =
            self.audio_dlg.is_some() || self.export_dlg.is_some() || self.new_dlg.is_some();
        if dialog_open {
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape)) {
                self.close_dialogs();
            }
            return;
        }
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        let (space, rec, home, end, save, fit) = ctx.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::NONE, Key::Space),
                i.consume_key(egui::Modifiers::NONE, Key::R),
                i.consume_key(egui::Modifiers::NONE, Key::Home)
                    || i.consume_key(egui::Modifiers::NONE, Key::Enter),
                i.consume_key(egui::Modifiers::NONE, Key::End),
                i.consume_key(egui::Modifiers::COMMAND, Key::S),
                i.consume_key(egui::Modifiers::NONE, Key::F),
            )
        });
        if space {
            if self.is_stopped() {
                self.play(ctx);
            } else {
                self.stop(ctx);
            }
        }
        if rec {
            if matches!(self.transport, Transport::Recording { .. }) {
                self.stop(ctx);
            } else {
                self.record(ctx);
            }
        }
        if home && self.is_stopped() {
            self.playhead = 0;
            self.view.scroll = 0.0;
        }
        if end && self.is_stopped() {
            self.playhead = self.project.end();
        }
        if save {
            self.save(ctx);
        }
        if fit {
            self.zoom_to_fit(ctx);
        }
    }

    fn close_dialogs(&mut self) {
        self.audio_dlg = None;
        self.export_dlg = None;
        self.new_dlg = None;
    }

    fn open_export(&mut self) {
        self.close_dialogs();
        self.export_dlg = Some(self.export_last.clone().unwrap_or_else(|| ExportSettings {
            sample_rate: self.project.sample_rate,
            depth: BitDepth::I24,
            stems: true,
            mix: false,
            dither: true,
            dir: self.project.dir.join("Export"),
        }));
    }

    fn open_audio(&mut self) {
        self.close_dialogs();
        self.audio_dlg = Some(AudioDialog::new(&self.prefs));
    }

    fn zoom_to_fit(&mut self, ctx: &egui::Context) {
        let w = (ctx.content_rect().width() - HEADER_W - 30.0).max(100.0) as f64;
        let end = self.project.end().max(self.sr() as u64 * 30) as f64;
        self.view.spp = (end * 1.05 / w).max(1.0);
        self.view.scroll = 0.0;
    }

    fn update_meters(&mut self, ctx: &egui::Context) {
        let now = ctx.input(|i| i.time);
        let dt = ctx.input(|i| i.stable_dt).min(0.1);
        let decay = (-dt * 1000.0 / 300.0).exp(); // ~ -26 dB/s
        for ch in 0..(self.engine.status.in_channels as usize).min(audio::MAX_INPUTS) {
            let p = self.engine.shared.take_meter(ch);
            self.meters[ch] = p.max(self.meters[ch] * decay);
            if p >= 0.999 {
                self.clip_hold[ch] = now + 2.0;
            }
        }
        if now - self.disk_checked > 2.0 {
            self.disk_checked = now;
            let mut p: &Path = &self.project.dir;
            while !p.exists() {
                match p.parent() {
                    Some(pp) => p = pp,
                    None => break,
                }
            }
            self.disk_free = fs4::available_space(p).ok();
        }
    }

    fn update_title(&mut self, ctx: &egui::Context) {
        let t = format!(
            "{}{} — TracksPlus",
            self.project.name,
            if self.project.dirty { " •" } else { "" }
        );
        if t != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(t.clone()));
            self.title = t;
        }
    }

    // ---------------------------------------------------------------- UI parts

    fn menu_bar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("Fichier", |ui| {
                if ui.button("Nouvelle session…").clicked() {
                    self.close_dialogs();
                    self.new_dlg = Some(NewSessionDialog {
                        name: chrono::Local::now()
                            .format("Session %Y-%m-%d %Hh%M")
                            .to_string(),
                        parent: self
                            .project
                            .dir
                            .parent()
                            .map(Path::to_path_buf)
                            .unwrap_or_else(project::default_sessions_root),
                        keep_tracks: true,
                    });
                }
                if ui.button("Ouvrir…").clicked() {
                    self.open_session(&ctx);
                }
                ui.separator();
                if ui.button("Enregistrer        Ctrl+S").clicked() {
                    self.save(&ctx);
                }
                if ui.button("Enregistrer sous…").clicked() {
                    self.save_as(&ctx);
                }
                if ui.button("Ouvrir le dossier de la session").clicked() {
                    let _ = self.project.ensure_dirs();
                    open_in_file_manager(&self.project.dir);
                }
                ui.separator();
                if ui
                    .add_enabled(self.project.has_audio(), egui::Button::new("Exporter…"))
                    .clicked()
                {
                    self.open_export();
                }
            });
            ui.menu_button("Pistes", |ui| {
                let enabled = self.is_stopped();
                if ui
                    .add_enabled(enabled, egui::Button::new("Ajouter une piste"))
                    .clicked()
                {
                    let i = self.next_free_input();
                    self.project.add_track(None, i);
                }
                if ui
                    .add_enabled(
                        enabled && self.engine.status.in_channels > 0,
                        egui::Button::new("Une piste par entrée de la carte"),
                    )
                    .clicked()
                {
                    self.add_track_per_input();
                }
                ui.separator();
                if ui
                    .add_enabled(enabled, egui::Button::new("Tout armer"))
                    .clicked()
                {
                    for t in &mut self.project.tracks {
                        t.armed = t.input.is_some();
                    }
                }
                if ui
                    .add_enabled(enabled, egui::Button::new("Tout désarmer"))
                    .clicked()
                {
                    for t in &mut self.project.tracks {
                        t.armed = false;
                    }
                }
                ui.separator();
                if ui
                    .add_enabled(
                        enabled && self.project.tracks.iter().any(|t| t.clips.is_empty()),
                        egui::Button::new("Supprimer les pistes vides"),
                    )
                    .clicked()
                {
                    self.project.tracks.retain(|t| !t.clips.is_empty());
                    self.project.dirty = true;
                }
            });
            ui.menu_button("Audio", |ui| {
                if ui.button("Réglages de la carte son…").clicked() {
                    self.open_audio();
                }
                ui.separator();
                ui.label("Format d'enregistrement");
                let enabled = self.is_stopped();
                for f in [RecFormat::Int24, RecFormat::Float32] {
                    if ui
                        .add_enabled(
                            enabled,
                            egui::RadioButton::new(self.project.rec_format == f, f.label()),
                        )
                        .clicked()
                    {
                        self.project.rec_format = f;
                        self.project.dirty = true;
                    }
                }
            });
            ui.menu_button("Affichage", |ui| {
                if ui.button("Tout afficher          F").clicked() {
                    self.zoom_to_fit(&ctx);
                }
                if ui.button("Zoom +").clicked() {
                    self.view.spp = (self.view.spp / 1.5).max(1.0);
                }
                if ui.button("Zoom −").clicked() {
                    self.view.spp = (self.view.spp * 1.5).min(1.0e6);
                }
            });
        });
    }

    fn transport_bar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let recording = matches!(self.transport, Transport::Recording { .. });
        let playing = matches!(self.transport, Transport::Playing { .. });
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            if transport_button(
                ui,
                Icon::ToStart,
                false,
                self.is_stopped(),
                "Retour au début (Origine)",
            )
            .clicked()
            {
                self.playhead = 0;
                self.view.scroll = 0.0;
            }
            if transport_button(ui, Icon::Stop, false, !self.is_stopped(), "Stop (Espace)")
                .clicked()
            {
                self.stop(&ctx);
            }
            if transport_button(ui, Icon::Play, playing, !recording, "Lecture (Espace)").clicked() {
                if playing {
                    self.stop(&ctx);
                } else {
                    self.play(&ctx);
                }
            }
            if transport_button(ui, Icon::Record, recording, true, "Enregistrer (R)").clicked() {
                if recording {
                    self.stop(&ctx);
                } else {
                    self.record(&ctx);
                }
            }
            ui.add_space(12.0);

            // Compteur
            let (rect, _) = ui.allocate_exact_size(vec2(190.0, 40.0), Sense::hover());
            ui.painter().rect_filled(rect, 6.0, theme::COUNTER_BG);
            let col = if recording { theme::REC } else { theme::TEXT };
            ui.painter().text(
                rect.center(),
                Align2::CENTER_CENTER,
                fmt_time(self.playhead, self.sr()),
                FontId::monospace(24.0),
                col,
            );
            ui.add_space(10.0);

            if recording {
                let blink = (ctx.input(|i| i.time) * 2.0).fract() < 0.6;
                let n = self.live.len();
                let txt = format!(
                    "{}  ENREGISTREMENT · {} piste{}",
                    if blink { "●" } else { " " },
                    n,
                    if n > 1 { "s" } else { "" }
                );
                ui.label(
                    egui::RichText::new(txt)
                        .color(theme::REC)
                        .strong()
                        .size(15.0),
                );
            } else {
                let armed = self.project.tracks.iter().filter(|t| t.armed).count();
                ui.label(
                    egui::RichText::new(format!("{armed} piste(s) armée(s)"))
                        .color(theme::TEXT_DIM)
                        .size(14.0),
                );
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(
                        self.project.has_audio() && self.is_stopped(),
                        egui::Button::new("⇪  Exporter"),
                    )
                    .clicked()
                {
                    self.open_export();
                }
                if ui.button("🔧  Carte son").clicked() {
                    self.open_audio();
                }
                if ui
                    .add_enabled(self.is_stopped(), egui::Button::new("＋  Piste"))
                    .clicked()
                {
                    let i = self.next_free_input();
                    self.project.add_track(None, i);
                }
            });
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        let st = &self.engine.status;
        ui.horizontal(|ui| {
            let dim = theme::TEXT_DIM;
            match &st.input {
                Some(name) => {
                    ui.label(egui::RichText::new("●").color(theme::OK));
                    ui.label(format!("{} · {}", st.host, name));
                }
                None => {
                    ui.label(egui::RichText::new("●").color(theme::REC));
                    ui.label("Aucune carte son");
                }
            }
            ui.separator();
            ui.label(
                egui::RichText::new(format!(
                    "{} kHz · {} entrées / {} sorties · buffer {}",
                    fmt_khz(st.sample_rate),
                    st.in_channels,
                    st.out_channels,
                    st.buffer.map_or("auto".to_string(), |b| b.to_string())
                ))
                .color(dim),
            );
            ui.separator();
            ui.label(
                egui::RichText::new(format!(
                    "Session {} kHz · {}",
                    fmt_khz(self.project.sample_rate),
                    self.project.rec_format.label()
                ))
                .color(dim),
            );

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let sh = &self.engine.shared;
                let xr = sh.xruns.load(Ordering::Relaxed) + sh.underruns.load(Ordering::Relaxed);
                let ov = sh.overruns.load(Ordering::Relaxed);
                if let Some(free) = self.disk_free {
                    let armed = self
                        .project
                        .tracks
                        .iter()
                        .filter(|t| t.armed && t.input.is_some())
                        .count()
                        .max(1);
                    let bps = match self.project.rec_format {
                        RecFormat::Int24 => 3,
                        RecFormat::Float32 => 4,
                    } * self.sr() as u64
                        * armed as u64;
                    let secs = free / bps.max(1);
                    let col = if secs < 1800 { theme::REC } else { dim };
                    ui.label(
                        egui::RichText::new(format!(
                            "Disque : {:.1} Go libres · ≈ {} d'enregistrement",
                            free as f64 / 1e9,
                            fmt_duration(secs)
                        ))
                        .color(col),
                    );
                }
                if xr > 0 || ov > 0 {
                    ui.separator();
                    let r = ui.label(
                        egui::RichText::new(format!("⚠ {xr} décrochages · {ov} pertes disque"))
                            .color(theme::WARN),
                    );
                    if r.on_hover_text("Cliquer pour remettre à zéro").clicked() {
                        sh.xruns.store(0, Ordering::Relaxed);
                        sh.underruns.store(0, Ordering::Relaxed);
                        sh.overruns.store(0, Ordering::Relaxed);
                    }
                }
                if let Some(e) = sh.last_error.lock().as_ref() {
                    ui.separator();
                    ui.label(egui::RichText::new(format!("⚠ {e}")).color(theme::REC));
                }
            });
        });
    }

    fn x_of(&self, rect: Rect, sample: u64) -> f32 {
        rect.left() + ((sample as f64 - self.view.scroll) / self.view.spp) as f32
    }

    fn sample_at(&self, rect: Rect, x: f32) -> u64 {
        (self.view.scroll + (x - rect.left()) as f64 * self.view.spp).max(0.0) as u64
    }

    fn ruler(&mut self, ui: &mut egui::Ui, rect: Rect) {
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, theme::RULER_BG);
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
        let mut t = (t0 / step).floor() * step;
        while t <= t1 {
            let x = self.x_of(rect, (t * sr) as u64);
            painter.line_segment(
                [pos2(x, rect.top() + 12.0), pos2(x, rect.bottom())],
                Stroke::new(1.0, theme::GRID_STRONG),
            );
            for k in 1..4 {
                let xs = x + (k as f64 * step / 4.0 * px_per_sec) as f32;
                painter.line_segment(
                    [pos2(xs, rect.bottom() - 5.0), pos2(xs, rect.bottom())],
                    Stroke::new(1.0, theme::GRID),
                );
            }
            painter.text(
                pos2(x + 4.0, rect.top() + 3.0),
                Align2::LEFT_TOP,
                fmt_ruler(t, step),
                FontId::monospace(11.0),
                theme::TEXT_DIM,
            );
            t += step;
        }
        // Tête de lecture
        let x = self.x_of(rect, self.playhead);
        if rect.x_range().contains(x) {
            let tri = vec![
                pos2(x - 6.0, rect.top()),
                pos2(x + 6.0, rect.top()),
                pos2(x, rect.top() + 9.0),
            ];
            painter.add(egui::Shape::convex_polygon(
                tri,
                theme::PLAYHEAD,
                Stroke::NONE,
            ));
        }
        let resp = ui.interact(
            rect,
            ui.make_persistent_id("ruler"),
            Sense::click_and_drag(),
        );
        if self.is_stopped()
            && (resp.clicked() || resp.dragged())
            && let Some(p) = resp.interact_pointer_pos()
        {
            self.playhead = self.sample_at(rect, p.x);
        }
    }

    fn timeline_input(&mut self, ui: &egui::Ui, rect: Rect) {
        let hovered = ui.rect_contains_pointer(rect);
        if !hovered {
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
            self.view.spp = (self.view.spp / zoom as f64).clamp(1.0, 1.0e6);
            self.view.scroll = (anchor - (px - rect.left()) as f64 * self.view.spp).max(0.0);
        }
        if scroll_x != 0.0 {
            self.view.scroll = (self.view.scroll - scroll_x as f64 * self.view.spp).max(0.0);
        }
    }

    fn follow_playhead(&mut self, width: f32) {
        if self.is_stopped() {
            return;
        }
        let visible = width as f64 * self.view.spp;
        let ph = self.playhead as f64;
        if ph > self.view.scroll + visible * 0.9 || ph < self.view.scroll {
            self.view.scroll = (ph - visible * 0.1).max(0.0);
        }
    }

    fn tracks_area(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let full = ui.available_rect_before_wrap();
        let tl_left = full.left() + HEADER_W;
        let ruler_rect = Rect::from_min_max(
            pos2(tl_left, full.top()),
            pos2(full.right(), full.top() + RULER_H),
        );
        let corner = Rect::from_min_max(full.min, pos2(tl_left, full.top() + RULER_H));
        ui.painter().rect_filled(corner, 0.0, theme::RULER_BG);
        ui.painter().text(
            corner.left_center() + vec2(12.0, 0.0),
            Align2::LEFT_CENTER,
            format!("{} pistes", self.project.tracks.len()),
            FontId::proportional(12.0),
            theme::TEXT_DIM,
        );
        self.follow_playhead(ruler_rect.width());
        self.ruler(ui, ruler_rect);
        let tl_all = Rect::from_min_max(pos2(tl_left, full.top()), full.max);
        self.timeline_input(ui, tl_all);

        let body = Rect::from_min_max(pos2(full.left(), full.top() + RULER_H), full.max);
        let mut child = ui.new_child(UiBuilder::new().max_rect(body));
        let track_h = 70.0;
        let mut to_delete = None;
        let mut clip_to_delete = None;
        let stopped = self.is_stopped();
        let in_ch = self.engine.status.in_channels;
        let now = ctx.input(|i| i.time);

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(&mut child, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                let ntracks = self.project.tracks.len();
                for idx in 0..ntracks {
                    let (row, _) =
                        ui.allocate_exact_size(vec2(ui.available_width(), track_h), Sense::hover());
                    if !ui.is_rect_visible(row) {
                        continue;
                    }
                    let color = theme::track_color(idx);
                    let header = Rect::from_min_size(row.min, vec2(HEADER_W, track_h));
                    let tl = Rect::from_min_max(pos2(row.left() + HEADER_W, row.top()), row.max);
                    let bg = if idx % 2 == 0 {
                        theme::ROW_A
                    } else {
                        theme::ROW_B
                    };
                    ui.painter().rect_filled(tl, 0.0, bg);
                    ui.painter().rect_filled(header, 0.0, theme::HEADER_BG);
                    ui.painter().rect_filled(
                        Rect::from_min_size(header.min, vec2(4.0, track_h)),
                        0.0,
                        color,
                    );
                    ui.painter().line_segment(
                        [
                            pos2(row.left(), row.bottom() - 0.5),
                            pos2(row.right(), row.bottom() - 0.5),
                        ],
                        Stroke::new(1.0, theme::SEPARATOR),
                    );

                    // ---- En-tête de piste
                    let track = &mut self.project.tracks[idx];
                    let mut hdr_ui = ui.new_child(
                        UiBuilder::new()
                            .max_rect(header.shrink2(vec2(10.0, 7.0)).translate(vec2(3.0, 0.0))),
                    );
                    hdr_ui.horizontal(|ui| {
                        if arm_button(ui, track.armed, stopped && track.input.is_some()).clicked() {
                            track.armed = !track.armed;
                        }
                        let r = ui.add_enabled(
                            stopped,
                            egui::TextEdit::singleline(&mut track.name)
                                .desired_width(150.0)
                                .font(FontId::proportional(14.0)),
                        );
                        if r.changed() {
                            self.project.dirty = true;
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .add_enabled(stopped, egui::Button::new("✕").frame(false))
                                .on_hover_text(
                                    "Supprimer la piste (les fichiers restent sur le disque)",
                                )
                                .clicked()
                            {
                                to_delete = Some(idx);
                            }
                        });
                    });
                    hdr_ui.add_space(4.0);
                    let track = &mut self.project.tracks[idx];
                    hdr_ui.horizontal(|ui| {
                        let label = match track.input {
                            Some(i) => format!("In {}", i + 1),
                            None => "—".into(),
                        };
                        ui.add_enabled_ui(stopped, |ui| {
                            egui::ComboBox::from_id_salt(("input", track.id))
                                .width(70.0)
                                .selected_text(label)
                                .height(400.0)
                                .show_ui(ui, |ui| {
                                    if ui
                                        .selectable_label(track.input.is_none(), "Aucune")
                                        .clicked()
                                    {
                                        track.input = None;
                                        track.armed = false;
                                    }
                                    for i in 0..in_ch {
                                        if ui
                                            .selectable_label(
                                                track.input == Some(i),
                                                format!("Entrée {}", i + 1),
                                            )
                                            .clicked()
                                        {
                                            track.input = Some(i);
                                        }
                                    }
                                });
                        });
                        let (mrect, _) = ui.allocate_exact_size(
                            vec2(ui.available_width() - 4.0, 12.0),
                            Sense::hover(),
                        );
                        let (lvl, clip) = match track.input {
                            Some(i) if (i as usize) < audio::MAX_INPUTS && i < in_ch => {
                                (self.meters[i as usize], self.clip_hold[i as usize] > now)
                            }
                            _ => (0.0, false),
                        };
                        draw_meter(ui.painter(), mrect, lvl, clip);
                    });

                    // ---- Timeline de la piste
                    let painter = ui.painter_at(tl);
                    let track = &self.project.tracks[idx];
                    let lane = tl.shrink2(vec2(0.0, 4.0));
                    for c in &track.clips {
                        let x0 = self.x_of(tl, c.start);
                        let x1 = self.x_of(tl, c.end());
                        if x1 < tl.left() || x0 > tl.right() {
                            continue;
                        }
                        let crect = Rect::from_min_max(
                            pos2(x0, lane.top()),
                            pos2(x1.max(x0 + 2.0), lane.bottom()),
                        );
                        draw_clip(
                            &painter,
                            crect,
                            tl,
                            color,
                            self.peaks.get(&c.id),
                            c.offset,
                            self.view.spp,
                            Some(&c.file),
                        );
                    }
                    // Prise en cours
                    if let Transport::Recording { start } = self.transport
                        && let Some(l) = self.live.iter().find(|l| l.track_id == track.id)
                    {
                        let x0 = self.x_of(tl, start);
                        let x1 = self.x_of(tl, self.playhead);
                        let crect = Rect::from_min_max(
                            pos2(x0, lane.top()),
                            pos2(x1.max(x0 + 2.0), lane.bottom()),
                        );
                        draw_clip(
                            &painter,
                            crect,
                            tl,
                            theme::REC,
                            Some(&l.peaks),
                            0,
                            self.view.spp,
                            None,
                        );
                    }

                    let resp =
                        ui.interact(tl, ui.make_persistent_id(("tl", track.id)), Sense::click());
                    if resp.clicked()
                        && stopped
                        && let Some(p) = resp.interact_pointer_pos()
                    {
                        self.playhead = self.sample_at(tl, p.x);
                    }
                    if resp.secondary_clicked() {
                        self.ctx_clip = resp.interact_pointer_pos().and_then(|p| {
                            let s = self.sample_at(tl, p.x);
                            track
                                .clips
                                .iter()
                                .rev()
                                .find(|c| c.start <= s && s < c.end())
                                .map(|c| (track.id, c.id))
                        });
                    }
                    let tid = track.id;
                    resp.context_menu(|ui| match self.ctx_clip {
                        Some((t, c)) if t == tid => {
                            if ui
                                .add_enabled(stopped, egui::Button::new("Supprimer cette prise"))
                                .clicked()
                            {
                                clip_to_delete = Some((t, c));
                                ui.close();
                            }
                        }
                        _ => {
                            ui.label(
                                egui::RichText::new("Aucune prise ici").color(theme::TEXT_DIM),
                            );
                        }
                    });
                }

                // Zone "ajouter"
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.add_space(12.0);
                    if ui
                        .add_enabled(stopped, egui::Button::new("＋  Ajouter une piste"))
                        .clicked()
                    {
                        let i = self.next_free_input();
                        self.project.add_track(None, i);
                    }
                    if ui
                        .add_enabled(
                            stopped && in_ch > 0,
                            egui::Button::new(format!("＋  Une piste par entrée ({in_ch})")),
                        )
                        .clicked()
                    {
                        self.add_track_per_input();
                    }
                });
                ui.add_space(20.0);
            });

        // Tête de lecture sur toute la hauteur
        let x = self.x_of(tl_all, self.playhead);
        if x >= tl_left && x <= full.right() {
            ui.painter().line_segment(
                [pos2(x, full.top() + RULER_H), pos2(x, full.bottom())],
                Stroke::new(1.5, theme::PLAYHEAD),
            );
        }

        if let Some(i) = to_delete {
            self.project.tracks.remove(i);
            self.project.dirty = true;
        }
        if let Some((t, c)) = clip_to_delete
            && let Some(track) = self.project.track_mut(t)
        {
            track.clips.retain(|x| x.id != c);
            self.project.dirty = true;
        }
    }

    // ---------------------------------------------------------------- dialogs

    fn audio_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dlg) = self.audio_dlg.take() else {
            return;
        };
        let mut open = true;
        let mut apply = false;
        egui::Window::new("Réglages de la carte son")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(440.0)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                egui::Grid::new("audio_grid").num_columns(2).spacing([14.0, 10.0]).show(ui, |ui| {
                    ui.label("Pilote");
                    let cur = dlg.prefs.host.clone().unwrap_or_default();
                    egui::ComboBox::from_id_salt("host").width(280.0).selected_text(&cur).show_ui(ui, |ui| {
                        for h in dlg.hosts.clone() {
                            if ui.selectable_label(cur == h, &h).clicked() {
                                dlg.prefs.host = Some(h);
                                dlg.prefs.input = None;
                                dlg.prefs.output = None;
                            }
                        }
                    });
                    ui.end_row();
                    if dlg.listed_host != dlg.prefs.host {
                        dlg.refresh();
                    }

                    ui.label("Entrée");
                    let cur = dlg.prefs.input.clone().unwrap_or_else(|| "Par défaut".into());
                    egui::ComboBox::from_id_salt("in").width(280.0).selected_text(&cur).show_ui(ui, |ui| {
                        if ui.selectable_label(dlg.prefs.input.is_none(), "Par défaut").clicked() {
                            dlg.prefs.input = None;
                        }
                        for d in &dlg.ins {
                            let txt = format!("{}  ({} canaux)", d.name, d.channels);
                            if ui.selectable_label(dlg.prefs.input.as_deref() == Some(&d.name), txt).clicked() {
                                dlg.prefs.input = Some(d.name.clone());
                                // Même carte en sortie si elle existe (cas ASIO / Dante).
                                if dlg.outs.iter().any(|o| o.name == d.name) {
                                    dlg.prefs.output = Some(d.name.clone());
                                }
                            }
                        }
                    });
                    ui.end_row();

                    ui.label("Sortie (écoute)");
                    let cur = dlg.prefs.output.clone().unwrap_or_else(|| "Par défaut".into());
                    egui::ComboBox::from_id_salt("out").width(280.0).selected_text(&cur).show_ui(ui, |ui| {
                        if ui.selectable_label(dlg.prefs.output.is_none(), "Par défaut").clicked() {
                            dlg.prefs.output = None;
                        }
                        for d in &dlg.outs {
                            let txt = format!("{}  ({} canaux)", d.name, d.channels);
                            if ui.selectable_label(dlg.prefs.output.as_deref() == Some(&d.name), txt).clicked() {
                                dlg.prefs.output = Some(d.name.clone());
                            }
                        }
                    });
                    ui.end_row();

                    let out_ch = dlg
                        .outs
                        .iter()
                        .find(|o| Some(&o.name) == dlg.prefs.output.as_ref())
                        .map(|o| o.channels)
                        .unwrap_or(2);
                    ui.label("Canaux d'écoute");
                    let l = dlg.prefs.out_left;
                    egui::ComboBox::from_id_salt("outpair")
                        .width(120.0)
                        .selected_text(format!("{} – {}", l + 1, l + 2))
                        .show_ui(ui, |ui| {
                            let mut i = 0;
                            while i < out_ch.max(2) {
                                if ui.selectable_label(l == i, format!("{} – {}", i + 1, i + 2)).clicked() {
                                    dlg.prefs.out_left = i;
                                }
                                i += 2;
                            }
                        });
                    ui.end_row();

                    // Fréquences communes à l'entrée choisie.
                    let rates: Vec<u32> = dlg
                        .ins
                        .iter()
                        .find(|d| Some(&d.name) == dlg.prefs.input.as_ref())
                        .or(dlg.ins.first())
                        .map(|d| d.rates.clone())
                        .filter(|r| !r.is_empty())
                        .unwrap_or_else(|| audio::COMMON_RATES.to_vec());
                    ui.label("Fréquence");
                    let cur = dlg.prefs.sample_rate.map_or("Par défaut".to_string(), |r| format!("{} kHz", fmt_khz(r)));
                    egui::ComboBox::from_id_salt("sr").width(120.0).selected_text(cur).show_ui(ui, |ui| {
                        if ui.selectable_label(dlg.prefs.sample_rate.is_none(), "Par défaut").clicked() {
                            dlg.prefs.sample_rate = None;
                        }
                        for r in rates {
                            if ui.selectable_label(dlg.prefs.sample_rate == Some(r), format!("{} kHz", fmt_khz(r))).clicked() {
                                dlg.prefs.sample_rate = Some(r);
                            }
                        }
                    });
                    ui.end_row();

                    ui.label("Taille de buffer");
                    let cur = dlg.prefs.buffer.map_or("Par défaut (pilote)".to_string(), |b| format!("{b} échantillons"));
                    egui::ComboBox::from_id_salt("buf").width(180.0).selected_text(cur).show_ui(ui, |ui| {
                        if ui.selectable_label(dlg.prefs.buffer.is_none(), "Par défaut (pilote)").clicked() {
                            dlg.prefs.buffer = None;
                        }
                        for b in audio::BUFFER_SIZES {
                            if ui.selectable_label(dlg.prefs.buffer == Some(b), format!("{b} échantillons")).clicked() {
                                dlg.prefs.buffer = Some(b);
                            }
                        }
                    });
                    ui.end_row();
                });

                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(
                        "Dante : sous Windows choisissez le pilote ASIO puis « Dante Virtual Soundcard » ; \
                         sous macOS, CoreAudio puis « Dante Virtual Soundcard ». La configuration est mémorisée.",
                    )
                    .color(theme::TEXT_DIM)
                    .size(12.0),
                );
                if let Some(e) = &dlg.error {
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new(format!("⚠ {e}")).color(theme::REC));
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Actualiser la liste").clicked() {
                        dlg.refresh();
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add(egui::Button::new(egui::RichText::new("Appliquer").strong()).fill(theme::ACCENT_DARK)).clicked() {
                            apply = true;
                        }
                    });
                });
            });
        if apply {
            match self.apply_audio(ctx, dlg.prefs.clone()) {
                Ok(()) => {
                    let st = &self.engine.status;
                    let msg = format!(
                        "{} · {} kHz · {} entrées",
                        st.input.clone().unwrap_or_else(|| "Pas d'entrée".into()),
                        fmt_khz(st.sample_rate),
                        st.in_channels
                    );
                    self.notify(ctx, msg, false);
                    return;
                }
                Err(e) => dlg.error = Some(e),
            }
        }
        if open {
            self.audio_dlg = Some(dlg);
        }
    }

    fn export_dialog(&mut self, ctx: &egui::Context) {
        if let Some(job) = &self.export_job {
            let p = job.progress.load(Ordering::Relaxed) as f32 / 1000.0;
            let mut cancel = false;
            egui::Window::new("Export en cours")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.add(
                        egui::ProgressBar::new(p)
                            .desired_width(320.0)
                            .show_percentage(),
                    );
                    ui.add_space(6.0);
                    if ui.button("Annuler").clicked() {
                        cancel = true;
                    }
                });
            if cancel {
                job.cancel.store(true, Ordering::Relaxed);
            }
            if job.is_finished() {
                let job = self.export_job.take().unwrap();
                match job.join() {
                    Ok(files) => {
                        let dir = files
                            .first()
                            .and_then(|f| f.parent())
                            .map(Path::to_path_buf);
                        self.notify(
                            ctx,
                            format!("Export terminé : {} fichier(s)", files.len()),
                            false,
                        );
                        if let Some(d) = dir {
                            open_in_file_manager(&d);
                        }
                    }
                    Err(e) => self.notify(ctx, format!("{e:#}"), true),
                }
            }
            return;
        }
        let Some(mut s) = self.export_dlg.take() else {
            return;
        };
        let mut open = true;
        let mut go = false;
        let n_tracks = self
            .project
            .tracks
            .iter()
            .filter(|t| !t.clips.is_empty())
            .count();
        egui::Window::new("Exporter")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(420.0)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                egui::Grid::new("exp_grid")
                    .num_columns(2)
                    .spacing([14.0, 10.0])
                    .show(ui, |ui| {
                        ui.label("Contenu");
                        ui.vertical(|ui| {
                            ui.checkbox(
                                &mut s.stems,
                                format!("Pistes séparées ({n_tracks} fichiers mono)"),
                            );
                            ui.checkbox(&mut s.mix, "Mix stéréo (somme des pistes)");
                        });
                        ui.end_row();

                        ui.label("Fréquence");
                        ui.horizontal(|ui| {
                            for r in [44100, 48000, 88200, 96000] {
                                ui.selectable_value(
                                    &mut s.sample_rate,
                                    r,
                                    format!("{} kHz", fmt_khz(r)),
                                );
                            }
                        });
                        ui.end_row();

                        ui.label("Résolution");
                        ui.horizontal(|ui| {
                            for d in BitDepth::ALL {
                                ui.selectable_value(&mut s.depth, d, d.label());
                            }
                        });
                        ui.end_row();

                        ui.label("");
                        ui.add_enabled(
                            s.depth == BitDepth::I16,
                            egui::Checkbox::new(&mut s.dither, "Dither (recommandé en 16 bits)"),
                        );
                        ui.end_row();

                        ui.label("Dossier");
                        ui.horizontal(|ui| {
                            let mut txt = s.dir.display().to_string();
                            if ui
                                .add(egui::TextEdit::singleline(&mut txt).desired_width(330.0))
                                .changed()
                            {
                                s.dir = PathBuf::from(txt);
                            }
                            if ui.button("…").clicked()
                                && let Some(d) = rfd::FileDialog::new()
                                    .set_directory(&self.project.dir)
                                    .pick_folder()
                            {
                                s.dir = d;
                            }
                        });
                        ui.end_row();
                    });
                ui.add_space(4.0);
                let secs = self.project.end() / self.project.sample_rate.max(1) as u64;
                ui.label(
                    egui::RichText::new(format!(
                        "Durée : {} · session en {} kHz{}",
                        fmt_duration(secs),
                        fmt_khz(self.project.sample_rate),
                        if s.sample_rate != self.project.sample_rate {
                            " → conversion de fréquence"
                        } else {
                            ""
                        }
                    ))
                    .color(theme::TEXT_DIM)
                    .size(12.0),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add_enabled(
                                s.stems || s.mix,
                                egui::Button::new(egui::RichText::new("Exporter").strong())
                                    .fill(theme::ACCENT_DARK),
                            )
                            .clicked()
                        {
                            go = true;
                        }
                    })
                });
            });
        if go {
            self.stop(ctx);
            self.export_last = Some(s.clone());
            self.export_job = Some(ExportJob::start(&self.project, s));
            return;
        }
        if open {
            self.export_dlg = Some(s);
        }
    }

    fn new_session_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut d) = self.new_dlg.take() else {
            return;
        };
        let mut open = true;
        let mut create = false;
        egui::Window::new("Nouvelle session")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                egui::Grid::new("new_grid")
                    .num_columns(2)
                    .spacing([14.0, 10.0])
                    .show(ui, |ui| {
                        ui.label("Nom");
                        ui.add(egui::TextEdit::singleline(&mut d.name).desired_width(260.0));
                        ui.end_row();
                        ui.label("Emplacement");
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(d.parent.display().to_string())
                                    .color(theme::TEXT_DIM),
                            );
                            if ui.button("…").clicked()
                                && let Some(p) = rfd::FileDialog::new()
                                    .set_directory(&d.parent)
                                    .pick_folder()
                            {
                                d.parent = p;
                            }
                        });
                        ui.end_row();
                        ui.label("");
                        ui.checkbox(
                            &mut d.keep_tracks,
                            "Reprendre les pistes et entrées actuelles",
                        );
                        ui.end_row();
                    });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(
                                egui::Button::new(egui::RichText::new("Créer").strong())
                                    .fill(theme::ACCENT_DARK),
                            )
                            .clicked()
                        {
                            create = true;
                        }
                    })
                });
            });
        if create && !d.name.trim().is_empty() {
            self.create_session(ctx, d);
            return;
        }
        if open {
            self.new_dlg = Some(d);
        }
    }

    fn toast_ui(&mut self, ctx: &egui::Context) {
        let now = ctx.input(|i| i.time);
        let Some(t) = &self.toast else {
            return;
        };
        if now > t.until {
            self.toast = None;
            return;
        }
        egui::Area::new(egui::Id::unique("toast"))
            .anchor(Align2::CENTER_BOTTOM, vec2(0.0, -40.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(if t.error {
                        theme::TOAST_ERR
                    } else {
                        theme::TOAST
                    })
                    .corner_radius(8.0)
                    .inner_margin(egui::Margin::symmetric(16, 10))
                    .show(ui, |ui| {
                        ui.set_max_width(600.0);
                        ui.label(
                            egui::RichText::new(&t.text)
                                .color(Color32::WHITE)
                                .size(14.0),
                        );
                    });
            });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.engine.collect_garbage();
        self.update_playhead();
        self.update_meters(&ctx);
        self.handle_keys(&ctx);
        self.update_title(&ctx);

        egui::Panel::top("menu")
            .frame(
                egui::Frame::new()
                    .fill(theme::PANEL_BG)
                    .inner_margin(egui::Margin::symmetric(8, 4)),
            )
            .show(ui, |ui| {
                self.menu_bar(ui);
            });
        egui::Panel::top("transport")
            .frame(
                egui::Frame::new()
                    .fill(theme::PANEL_BG)
                    .inner_margin(egui::Margin::symmetric(12, 8)),
            )
            .show(ui, |ui| self.transport_bar(ui));
        egui::Panel::bottom("status")
            .frame(
                egui::Frame::new()
                    .fill(theme::PANEL_BG)
                    .inner_margin(egui::Margin::symmetric(12, 5)),
            )
            .show(ui, |ui| self.status_bar(ui));
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::BG))
            .show(ui, |ui| self.tracks_area(ui));

        self.audio_dialog(&ctx);
        self.export_dialog(&ctx);
        self.new_session_dialog(&ctx);
        self.toast_ui(&ctx);

        ctx.request_repaint_after(std::time::Duration::from_millis(33));
    }

    fn on_exit(&mut self) {
        let ctx = egui::Context::default();
        self.stop(&ctx);
        self.autosave();
    }
}

// -------------------------------------------------------------------- widgets

#[derive(Clone, Copy)]
enum Icon {
    ToStart,
    Stop,
    Play,
    Record,
}

fn transport_button(
    ui: &mut egui::Ui,
    icon: Icon,
    active: bool,
    enabled: bool,
    tip: &str,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(
        vec2(46.0, 40.0),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    let resp = resp.on_hover_text(tip);
    let hovered = resp.hovered() && enabled;
    let (bg, fg) = match (icon, active) {
        (Icon::Record, true) => (theme::REC, Color32::WHITE),
        (Icon::Play, true) => (theme::PLAY, Color32::WHITE),
        _ => (
            if hovered {
                theme::BUTTON_HOVER
            } else {
                theme::BUTTON
            },
            match icon {
                Icon::Record => theme::REC,
                Icon::Play => theme::PLAY,
                _ => theme::TEXT,
            },
        ),
    };
    let fg = if enabled { fg } else { fg.gamma_multiply(0.35) };
    let p = ui.painter();
    p.rect_filled(rect, 7.0, bg);
    let c = rect.center();
    match icon {
        Icon::Play => {
            let pts = vec![
                c + vec2(-6.0, -9.0),
                c + vec2(10.0, 0.0),
                c + vec2(-6.0, 9.0),
            ];
            p.add(egui::Shape::convex_polygon(pts, fg, Stroke::NONE));
        }
        Icon::Stop => {
            p.rect_filled(Rect::from_center_size(c, vec2(15.0, 15.0)), 2.0, fg);
        }
        Icon::Record => {
            p.circle_filled(c, 9.0, fg);
        }
        Icon::ToStart => {
            p.rect_filled(
                Rect::from_center_size(c + vec2(-8.0, 0.0), vec2(3.0, 16.0)),
                0.0,
                fg,
            );
            let pts = vec![c + vec2(8.0, -8.0), c + vec2(8.0, 8.0), c + vec2(-5.0, 0.0)];
            p.add(egui::Shape::convex_polygon(pts, fg, Stroke::NONE));
        }
    }
    resp
}

fn arm_button(ui: &mut egui::Ui, armed: bool, enabled: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(
        vec2(24.0, 24.0),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    let resp = resp.on_hover_text("Armer la piste pour l'enregistrement");
    let p = ui.painter();
    let bg = if armed {
        theme::REC
    } else if resp.hovered() && enabled {
        theme::BUTTON_HOVER
    } else {
        theme::BUTTON
    };
    p.circle_filled(rect.center(), 11.0, bg);
    let dot = if armed {
        Color32::WHITE
    } else {
        theme::REC.gamma_multiply(if enabled { 0.9 } else { 0.35 })
    };
    p.circle_filled(rect.center(), 4.5, dot);
    resp
}

fn draw_meter(p: &egui::Painter, rect: Rect, level: f32, clip: bool) {
    p.rect_filled(rect, 2.0, theme::METER_BG);
    let db = if level > 1e-6 {
        20.0 * level.log10()
    } else {
        -120.0
    };
    let norm = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
    if norm > 0.0 {
        let w = rect.width() * norm;
        let col = if db > -3.0 {
            theme::REC
        } else if db > -12.0 {
            theme::WARN
        } else {
            theme::OK
        };
        p.rect_filled(
            Rect::from_min_size(rect.min, vec2(w, rect.height())),
            2.0,
            col,
        );
    }
    // Graduations -48, -24, -12, -6 dB
    for g in [-48.0f32, -24.0, -12.0, -6.0] {
        let x = rect.left() + rect.width() * (g + 60.0) / 60.0;
        p.line_segment(
            [pos2(x, rect.top()), pos2(x, rect.bottom())],
            Stroke::new(1.0, Color32::from_black_alpha(90)),
        );
    }
    if clip {
        p.rect_filled(
            Rect::from_min_max(pos2(rect.right() - 5.0, rect.top()), rect.max),
            1.0,
            theme::REC,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_clip(
    painter: &egui::Painter,
    rect: Rect,
    visible: Rect,
    color: Color32,
    peaks: Option<&Peaks>,
    offset: u64,
    spp: f64,
    label: Option<&str>,
) {
    painter.rect_filled(rect, CornerRadius::same(4), color.gamma_multiply(0.22));
    painter.rect_stroke(
        rect,
        CornerRadius::same(4),
        Stroke::new(1.0, color.gamma_multiply(0.8)),
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
            let mid = rect.center().y;
            let half = rect.height() * 0.45;
            let x0 = rect.left().max(visible.left()).floor();
            let x1 = rect.right().min(visible.right()).ceil();
            let mut mesh = Mesh::default();
            let wcol = color.gamma_multiply(0.95);
            let mut x = x0;
            while x < x1 {
                let s0 = offset as f64 + (x - rect.left()) as f64 * spp;
                let s1 = s0 + spp;
                let i0 = (s0 / per) as usize;
                let i1 = ((s1 / per) as usize).max(i0 + 1).min(lvl.len());
                if i0 >= lvl.len() {
                    break;
                }
                let (mut mn, mut mx) = (i16::MAX, i16::MIN);
                for p in &lvl[i0..i1] {
                    mn = mn.min(p[0]);
                    mx = mx.max(p[1]);
                }
                let top = mid - (mx as f32 / 32767.0) * half;
                let bot = mid - (mn as f32 / 32767.0) * half;
                let r = Rect::from_min_max(
                    pos2(x, top.min(mid - 0.5)),
                    pos2(x + 1.0, bot.max(mid + 0.5)),
                );
                mesh.add_colored_rect(r, wcol);
                x += 1.0;
            }
            painter.add(egui::Shape::mesh(mesh));
        }
    }
    if let Some(label) = label
        && rect.width() > 60.0
    {
        let pos = Pos2::new(rect.left().max(visible.left()) + 5.0, rect.top() + 3.0);
        painter.text(
            pos,
            Align2::LEFT_TOP,
            label,
            FontId::proportional(10.5),
            color.gamma_multiply(1.0),
        );
    }
}

// -------------------------------------------------------------------- helpers

pub fn fmt_time(samples: u64, sr: u32) -> String {
    let sr = sr.max(1) as u64;
    let ms_total = samples * 1000 / sr;
    let h = ms_total / 3_600_000;
    let m = (ms_total / 60_000) % 60;
    let s = (ms_total / 1000) % 60;
    let ms = ms_total % 1000;
    format!("{h:02}:{m:02}:{s:02}.{ms:03}")
}

fn fmt_ruler(t: f64, step: f64) -> String {
    let total = t.round() as u64;
    let (m, s) = (total / 60, total % 60);
    if step < 1.0 {
        format!("{}:{:04.1}", (t / 60.0).floor() as u64, t % 60.0)
    } else if m >= 60 {
        format!("{}:{:02}:{:02}", m / 60, m % 60, s)
    } else {
        format!("{m}:{s:02}")
    }
}

fn fmt_khz(sr: u32) -> String {
    if sr.is_multiple_of(1000) {
        format!("{}", sr / 1000)
    } else {
        format!("{:.1}", sr as f64 / 1000.0)
    }
}

fn fmt_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs / 60) % 60;
    if h > 0 {
        format!("{h} h {m:02}")
    } else {
        format!("{m} min {:02}", secs % 60)
    }
}

fn open_in_file_manager(path: &Path) {
    #[cfg(target_os = "windows")]
    let cmd = "explorer";
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    let cmd = "xdg-open";
    let _ = std::process::Command::new(cmd).arg(path).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_format() {
        assert_eq!(fmt_time(48000 * 61 + 24000, 48000), "00:01:01.500");
        assert_eq!(fmt_khz(44100), "44.1");
        assert_eq!(fmt_khz(48000), "48");
        assert_eq!(fmt_duration(3725), "1 h 02");
    }
}
