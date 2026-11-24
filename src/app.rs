//! Application : état global, transport, sessions, barre supérieure et fenêtres.
//! Les onglets sont dans `ui::multitrack` et `ui::soundboard`.

use crate::audio::{self, AudioPrefs, DeviceInfo, Engine, RecTarget, Shared};
use crate::export::{BitDepth, ExportJob, ExportSettings};
use crate::midi::Midi;
use crate::player::Player;
use crate::project::{self, Clip, PROJECT_EXT, Project, RecFormat};
use crate::recorder::{RecSpec, Recorder};
use crate::render::{self, PeakData, Peaks};
use crate::sampler::PadCmd;
use crate::soundboard::{Board, SampleCache, db_to_gain};
use crate::ui::theme as t;
use crate::ui::widgets::{self as w, BtnStyle, Icon, MeterState, sq};
use eframe::egui::{self, Align2, Color32, Key, Vec2, vec2};
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Transport {
    Stopped,
    Playing { start: u64 },
    Recording { start: u64 },
}

pub struct LiveTake {
    pub track_id: u64,
    pub peaks: Peaks,
}

pub struct View {
    /// Échantillons par pixel.
    pub spp: f64,
    /// Échantillon au bord gauche de la timeline.
    pub scroll: f64,
    pub track_h: f32,
    /// Suivi automatique de la tête de lecture.
    pub follow: bool,
    /// Le suivi reprend après cet instant (l'utilisateur navigue).
    pub nav_until: f64,
    /// Défilement vertical demandé.
    pub want_scroll_y: Option<f32>,
    pub scroll_y: f32,
    pub viewport_h: f32,
    pub width: f32,
}

pub struct AudioDialog {
    pub prefs: AudioPrefs,
    pub hosts: Vec<String>,
    pub ins: Vec<DeviceInfo>,
    pub outs: Vec<DeviceInfo>,
    pub listed_host: Option<String>,
    pub error: Option<String>,
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

pub struct NewSessionDialog {
    pub name: String,
    pub parent: PathBuf,
    pub keep_tracks: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RangeChoice {
    All,
    Selection,
}

pub struct ExportDialog {
    pub s: ExportSettings,
    pub range: RangeChoice,
    pub tracks: HashSet<u64>,
}

struct Toast {
    text: String,
    error: bool,
    until: f64,
}

/// État de l'onglet soundboard.
#[derive(Default)]
pub struct SbState {
    pub page: usize,
    pub selected: Option<u16>,
    pub learn_midi: Option<u16>,
    pub learn_key: Option<u16>,
    /// Pad maintenu à la souris.
    pub mouse_down: Option<u16>,
    /// Touches clavier maintenues → pad.
    pub held_keys: HashMap<Key, u16>,
    /// Zoom de l'éditeur (fractions de la durée).
    pub zoom: (f64, f64),
    pub drag: Option<crate::ui::soundboard::Handle>,
    pub midi_seen: f64,
    pub midi_last: Option<String>,
    pub midi_dirty: bool,
    pub flash: HashMap<u16, f64>,
}

pub struct App {
    pub tab: usize,
    pub project: Project,
    pub engine: Engine,
    pub prefs: AudioPrefs,
    pub transport: Transport,
    pub playhead: u64,
    pub player: Option<Player>,
    pub recorder: Option<Recorder>,
    pub live: Vec<LiveTake>,
    pub peaks: HashMap<u64, Peaks>,
    pub in_meters: Vec<MeterState>,
    pub master_play: [MeterState; 2],
    pub master_pad: [MeterState; 2],
    pub view: View,
    pub selection: Option<(u64, u64)>,
    pub selected_tracks: HashSet<u64>,
    pub show_bridge: bool,
    pub ruler_anchor: Option<u64>,
    pub audio_dlg: Option<AudioDialog>,
    pub export_dlg: Option<ExportDialog>,
    pub export_job: Option<ExportJob>,
    pub new_dlg: Option<NewSessionDialog>,
    toast: Option<Toast>,
    pub ctx_clip: Option<(u64, u64)>,
    pub disk_free: Option<u64>,
    disk_checked: f64,
    title: String,
    pub export_last: Option<ExportSettings>,
    pub now: f64,
    // Soundboard
    pub board: Board,
    pub board_path: PathBuf,
    pub board_changed: Option<f64>,
    pub cache: SampleCache,
    pub midi: Midi,
    pub midi_ports: Vec<String>,
    pub sb: SbState,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        t::apply(&cc.egui_ctx);
        let prefs = AudioPrefs::load();
        let board = Board::load_default();
        let (engine, err) = match Engine::open(&prefs, board.out_left) {
            Ok(e) => (e, None),
            Err(e) => (Engine::offline(), Some(format!("{e:#}"))),
        };
        let mut project = Project::untitled(engine.status.sample_rate);
        let n = engine.status.in_channels.clamp(1, 2);
        for i in 0..n {
            project.add_track(None, Some(i));
        }
        project.dirty = false;
        let cache = SampleCache::new(engine.status.sample_rate);
        let mut app = Self {
            tab: 0,
            project,
            engine,
            prefs,
            transport: Transport::Stopped,
            playhead: 0,
            player: None,
            recorder: None,
            live: Vec::new(),
            peaks: HashMap::new(),
            in_meters: vec![MeterState::default(); audio::MAX_INPUTS],
            master_play: Default::default(),
            master_pad: Default::default(),
            view: View {
                spp: 1200.0,
                scroll: 0.0,
                track_h: 76.0,
                follow: true,
                nav_until: 0.0,
                want_scroll_y: None,
                scroll_y: 0.0,
                viewport_h: 400.0,
                width: 800.0,
            },
            selection: None,
            selected_tracks: HashSet::new(),
            show_bridge: false,
            ruler_anchor: None,
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
            now: 0.0,
            board_path: Board::default_path().unwrap_or_else(|| PathBuf::from("soundboard.json")),
            board,
            board_changed: None,
            cache,
            midi: Midi::new(),
            midi_ports: crate::midi::list_ports(),
            sb: SbState {
                zoom: (0.0, 1.0),
                midi_dirty: true,
                ..Default::default()
            },
        };
        app.midi.shared.lock().ctx = Some(cc.egui_ctx.clone());
        let port = app.board.midi_port.clone();
        app.midi.connect(port.as_deref());
        app.sync_engine_to_board();
        let paths: Vec<PathBuf> = app.board.all_pads().map(|(_, p)| p.path.clone()).collect();
        for p in paths {
            app.cache.request(&p, Some(&cc.egui_ctx));
        }
        if let Some(e) = err {
            let mut d = AudioDialog::new(&app.prefs);
            d.error = Some(e);
            app.audio_dlg = Some(d);
        }
        app
    }

    pub fn notify(&mut self, text: impl Into<String>, error: bool) {
        self.toast = Some(Toast {
            text: text.into(),
            error,
            until: self.now + if error { 8.0 } else { 3.5 },
        });
    }

    pub fn sr(&self) -> u32 {
        self.engine.status.sample_rate.max(1)
    }

    /// Envoie au moteur les réglages du soundboard (sortie, volume) et au MIDI le canal.
    pub fn sync_engine_to_board(&mut self) {
        let out_ch = self.engine.status.out_channels.max(1) as usize;
        self.engine.pad(PadCmd::SetOutput {
            left: (self.board.out_left as usize).min(out_ch - 1),
        });
        self.engine.pad(PadCmd::SetMaster {
            gain: db_to_gain(self.board.master_db),
        });
        self.midi.shared.lock().tx = self.engine.pad_tx.clone();
        self.sb.midi_dirty = true;
    }

    pub fn board_modified(&mut self) {
        self.board_changed = Some(self.now);
        self.sb.midi_dirty = true;
    }

    // ---------------------------------------------------------------- transport

    pub fn is_stopped(&self) -> bool {
        self.transport == Transport::Stopped
    }

    pub fn is_recording(&self) -> bool {
        matches!(self.transport, Transport::Recording { .. })
    }

    pub fn play(&mut self) {
        if !self.is_stopped() {
            return;
        }
        if !self.engine.has_output() {
            self.notify(
                "Aucune sortie audio : choisissez-en une dans les réglages audio",
                true,
            );
            return;
        }
        if self.playhead >= self.project.end() && self.project.end() > 0 {
            self.playhead = self.selection.map_or(0, |s| s.0);
        }
        let (player, cons) = Player::start(&self.project, self.playhead, self.project.audible());
        self.engine.start_playback(cons);
        self.player = Some(player);
        self.transport = Transport::Playing {
            start: self.playhead,
        };
        self.view.nav_until = 0.0;
    }

    /// Déplace la tête de lecture ; en lecture, la lecture repart de là immédiatement.
    pub fn seek(&mut self, pos: u64) {
        match self.transport {
            Transport::Stopped => self.playhead = pos,
            Transport::Playing { .. } => {
                self.playhead = pos;
                let (player, cons) = Player::start(&self.project, pos, self.project.audible());
                self.engine.start_playback(cons);
                self.player = Some(player);
                self.transport = Transport::Playing { start: pos };
            }
            Transport::Recording { .. } => {}
        }
    }

    pub fn update_audible(&mut self) {
        if let Some(p) = &self.player {
            for (a, v) in p.audible.iter().zip(self.project.audible()) {
                a.store(v, Ordering::Relaxed);
            }
        }
    }

    pub fn record(&mut self) {
        if !self.is_stopped() {
            if matches!(self.transport, Transport::Playing { .. }) {
                self.stop();
            } else {
                return;
            }
        }
        if !self.engine.has_input() {
            self.notify(
                "Aucune entrée audio : choisissez votre carte dans les réglages audio",
                true,
            );
            return;
        }
        let sr = self.engine.status.sample_rate;
        if self.project.has_audio() && self.project.sample_rate != sr {
            self.notify(
                format!(
                    "La session est en {} kHz mais la carte tourne en {} kHz. Changez la fréquence dans les réglages audio.",
                    fmt_khz(self.project.sample_rate),
                    fmt_khz(sr)
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
                "Armez au moins une piste (pastille rouge) avec une entrée valide",
                true,
            );
            return;
        }
        self.project.sample_rate = sr;
        if let Err(e) = self.project.ensure_dirs() {
            self.notify(format!("{e:#}"), true);
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
            self.notify(format!("{e:#}"), true);
            return;
        }
        if self.engine.has_output() && self.project.has_audio() {
            let (player, cons) =
                Player::start(&self.project, self.playhead, self.project.audible());
            self.engine.start_playback(cons);
            self.player = Some(player);
        }
        self.recorder = Some(recorder);
        self.transport = Transport::Recording {
            start: self.playhead,
        };
        self.view.nav_until = 0.0;
        self.reveal_recording_tracks();
    }

    pub fn stop(&mut self) {
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
                    if let Some(e) = &r.error {
                        errors.push(e.clone());
                    }
                    if r.frames == 0 {
                        let _ = std::fs::remove_file(self.project.audio_dir().join(&r.file));
                        continue;
                    }
                    max_len = max_len.max(r.frames);
                    let id = self.project.alloc_id();
                    if let Some(l) = self.live.iter().find(|l| l.track_id == r.track_id) {
                        self.peaks.insert(id, l.peaks.clone());
                    }
                    if let Some(tr) = self.project.track_mut(r.track_id) {
                        tr.clips.push(Clip {
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
                        format!("Erreur d'enregistrement : {}", errors.join(" ; ")),
                        true,
                    );
                } else if overruns > 0 {
                    self.notify(
                        format!("Attention : {overruns} pertes de données (disque trop lent ?)"),
                        true,
                    );
                } else if let Err(e) = save {
                    self.notify(format!("Sauvegarde impossible : {e:#}"), true);
                } else {
                    self.notify(
                        format!("Prise enregistrée · {}", fmt_time(max_len, self.sr())),
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

    /// Fait défiler verticalement si aucune piste en cours d'enregistrement n'est visible.
    pub fn reveal_recording_tracks(&mut self) {
        let h = self.view.track_h;
        let rows: Vec<usize> = self
            .project
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, t)| self.live.iter().any(|l| l.track_id == t.id))
            .map(|(i, _)| i)
            .collect();
        let Some(&first) = rows.first() else {
            return;
        };
        let top = self.view.scroll_y;
        let bottom = top + self.view.viewport_h;
        let visible = rows.iter().any(|&i| {
            let y = i as f32 * h + h * 0.5;
            y >= top && y <= bottom
        });
        if !visible {
            self.view.want_scroll_y = Some((first as f32 * h - 8.0).max(0.0));
        }
    }

    pub fn add_marker(&mut self) {
        let pos = self.playhead;
        self.project.add_marker(pos);
        let n = self.project.markers.len();
        self.notify(format!("Repère {n} · {}", fmt_time(pos, self.sr())), false);
    }

    // ---------------------------------------------------------------- session

    fn load_peaks(&mut self) {
        self.peaks.clear();
        for tr in &self.project.tracks {
            for c in &tr.clips {
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
        self.save_board_now();
    }

    pub fn save_board_now(&mut self) {
        if self.board_changed.take().is_some()
            && let Err(e) = self.board.save(&self.board_path.clone())
        {
            self.notify(format!("Soundboard non sauvegardé : {e:#}"), true);
        }
    }

    fn reset_view_for_session(&mut self) {
        self.playhead = 0;
        self.view.scroll = 0.0;
        self.view.want_scroll_y = Some(0.0);
        self.selection = None;
        self.selected_tracks.clear();
    }

    pub fn open_session(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .set_title("Ouvrir une session")
            .add_filter("Session TracksPlus", &[PROJECT_EXT])
            .set_directory(project::default_sessions_root())
            .pick_file()
        else {
            return;
        };
        self.stop();
        self.autosave();
        match Project::load(&path) {
            Ok(p) => {
                self.project = p;
                self.reset_view_for_session();
                self.load_peaks();
                if self.project.has_audio()
                    && self.project.sample_rate != self.engine.status.sample_rate
                {
                    let msg = format!(
                        "Session en {} kHz, carte en {} kHz : réglez la carte sur {} kHz.",
                        fmt_khz(self.project.sample_rate),
                        fmt_khz(self.engine.status.sample_rate),
                        fmt_khz(self.project.sample_rate)
                    );
                    self.notify(msg, true);
                } else {
                    self.notify(format!("« {} » ouverte", self.project.name), false);
                }
            }
            Err(e) => self.notify(format!("{e:#}"), true),
        }
    }

    pub fn save(&mut self) {
        match self.project.save() {
            Ok(()) => self.notify(
                format!("Enregistré · {}", self.project.dir.display()),
                false,
            ),
            Err(e) => self.notify(format!("{e:#}"), true),
        }
    }

    pub fn save_as(&mut self) {
        let Some(parent) = rfd::FileDialog::new()
            .set_title("Dossier où créer la session")
            .set_directory(project::default_sessions_root())
            .pick_folder()
        else {
            return;
        };
        let name = self.project.name.clone();
        let dir = parent.join(project::sanitize(&name));
        match self.project.save_as(dir, name) {
            Ok(()) => self.notify(
                format!("Session enregistrée · {}", self.project.dir.display()),
                false,
            ),
            Err(e) => self.notify(format!("{e:#}"), true),
        }
    }

    fn create_session(&mut self, d: NewSessionDialog) {
        self.stop();
        self.autosave();
        let name = project::sanitize(&d.name);
        let dir = d.parent.join(&name);
        if dir.join(format!("{name}.{PROJECT_EXT}")).exists() {
            self.notify(format!("Une session « {name} » existe déjà ici"), true);
            return;
        }
        let mut p = Project::new(name, dir, self.engine.status.sample_rate);
        p.rec_format = self.project.rec_format;
        if d.keep_tracks {
            for tr in &self.project.tracks {
                let id = p.add_track(Some(tr.name.clone()), tr.input);
                if let Some(nt) = p.track_mut(id) {
                    nt.armed = tr.armed;
                }
            }
        }
        if let Err(e) = p.save() {
            self.notify(format!("{e:#}"), true);
            return;
        }
        self.project = p;
        self.peaks.clear();
        self.reset_view_for_session();
        self.notify(format!("Nouvelle session « {} »", self.project.name), false);
    }

    pub fn add_track(&mut self) {
        let used: HashSet<u16> = self.project.tracks.iter().filter_map(|t| t.input).collect();
        let free = (0..self.engine.status.in_channels).find(|i| !used.contains(i));
        self.project.add_track(None, free);
    }

    pub fn add_track_per_input(&mut self) {
        let used: HashSet<u16> = self.project.tracks.iter().filter_map(|t| t.input).collect();
        for i in 0..self.engine.status.in_channels {
            if !used.contains(&i) {
                self.project
                    .add_track(Some(format!("Entrée {}", i + 1)), Some(i));
            }
        }
    }

    fn apply_audio(&mut self, prefs: AudioPrefs) -> Result<(), String> {
        self.stop();
        self.engine.pad(PadCmd::StopAll { fade: 0 });
        // Libère d'abord l'ancien pilote (ASIO n'en autorise qu'un à la fois).
        self.engine = Engine::offline();
        let res = match Engine::open(&prefs, self.board.out_left) {
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
                if let Ok(old) = Engine::open(&self.prefs, self.board.out_left) {
                    self.engine = old;
                }
                Err(format!("{e:#}"))
            }
        };
        self.cache.set_sample_rate(self.engine.status.sample_rate);
        self.sync_engine_to_board();
        res
    }

    pub fn open_export(&mut self) {
        if !self.project.has_audio() {
            self.notify("Rien à exporter pour l'instant", true);
            return;
        }
        self.close_dialogs();
        let s = self.export_last.clone().unwrap_or_else(|| ExportSettings {
            sample_rate: self.project.sample_rate,
            depth: BitDepth::I24,
            stems: true,
            mix: false,
            dither: true,
            dir: self.project.dir.join("Export"),
            tracks: Vec::new(),
            range: None,
        });
        let with_audio: HashSet<u64> = self
            .project
            .tracks
            .iter()
            .filter(|t| !t.clips.is_empty())
            .map(|t| t.id)
            .collect();
        let sel: HashSet<u64> = self
            .selected_tracks
            .intersection(&with_audio)
            .copied()
            .collect();
        self.export_dlg = Some(ExportDialog {
            s,
            range: if self.selection.is_some() {
                RangeChoice::Selection
            } else {
                RangeChoice::All
            },
            tracks: if sel.is_empty() { with_audio } else { sel },
        });
    }

    pub fn open_audio(&mut self) {
        self.close_dialogs();
        self.audio_dlg = Some(AudioDialog::new(&self.prefs));
    }

    pub fn open_new_session(&mut self) {
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

    fn close_dialogs(&mut self) {
        self.audio_dlg = None;
        self.export_dlg = None;
        self.new_dlg = None;
    }

    pub fn dialog_open(&self) -> bool {
        self.audio_dlg.is_some()
            || self.export_dlg.is_some()
            || self.new_dlg.is_some()
            || self.export_job.is_some()
    }

    pub fn zoom_to_fit(&mut self) {
        let w = self.view.width.max(100.0) as f64;
        let end = self.project.end().max(self.sr() as u64 * 30) as f64;
        self.view.spp = (end * 1.05 / w).max(1.0);
        self.view.scroll = 0.0;
        self.view.nav_until = self.now + 3.0;
    }

    // ---------------------------------------------------------------- frame

    fn global_keys(&mut self, ctx: &egui::Context) {
        if self.dialog_open() {
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape))
                && self.export_job.is_none()
            {
                self.close_dialogs();
            }
            return;
        }
        let (save, export, tab1, tab2) = ctx.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::COMMAND, Key::S),
                i.consume_key(egui::Modifiers::COMMAND, Key::E),
                i.consume_key(egui::Modifiers::COMMAND, Key::Num1),
                i.consume_key(egui::Modifiers::COMMAND, Key::Num2),
            )
        });
        if save {
            self.save();
        }
        if export {
            self.open_export();
        }
        if tab1 {
            self.tab = 0;
        }
        if tab2 {
            self.tab = 1;
        }
    }

    fn update_meters(&mut self, ctx: &egui::Context) {
        let dt = ctx.input(|i| i.stable_dt).min(0.1);
        let now = self.now;
        let sh = &self.engine.shared;
        for ch in 0..(self.engine.status.in_channels as usize).min(audio::MAX_INPUTS) {
            let p = sh.take_meter(ch);
            self.in_meters[ch].update(p, now, dt);
        }
        for k in 0..2 {
            self.master_play[k].update(Shared::take_peak(&sh.master_play[k]), now, dt);
            self.master_pad[k].update(Shared::take_peak(&sh.master_pad[k]), now, dt);
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
        let tt = format!(
            "{}{} — TracksPlus",
            self.project.name,
            if self.project.dirty { " •" } else { "" }
        );
        if tt != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(tt.clone()));
            self.title = tt;
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let full = ui.max_rect();
        ui.horizontal_centered(|ui| {
            ui.add_space(4.0);
            let more = w::icon_button(ui, Icon::More, sq(30.0), BtnStyle::Ghost, true, "Menu");
            egui::Popup::menu(&more)
                .width(250.0)
                .show(|ui| self.main_menu(ui));
            ui.add_space(4.0);
            ui.vertical(|ui| {
                ui.add_space(10.0);
                ui.spacing_mut().item_spacing.y = 1.0;
                ui.label(
                    egui::RichText::new(short(&self.project.name, 34))
                        .font(t::semibold(13.5))
                        .color(t::TEXT),
                );
                let sub = if self.project.dirty {
                    "Modifications non enregistrées"
                } else {
                    "Enregistrée"
                };
                ui.label(
                    egui::RichText::new(sub)
                        .font(t::regular(11.0))
                        .color(t::TEXT_3),
                );
            });
        });

        // Onglets centrés.
        let seg_w = 130.0;
        let r = egui::Rect::from_center_size(full.center(), vec2(seg_w * 2.0 + 6.0, 34.0));
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(r));
        let mut tab = self.tab;
        if w::segmented(
            &mut child,
            &mut tab,
            &["Multipiste", "Soundboard"],
            seg_w,
            34.0,
        ) {
            self.tab = tab;
        }

        // Carte son à droite.
        let st = &self.engine.status;
        let ok = st.input.is_some() || st.output.is_some();
        let dev = st
            .input
            .clone()
            .or(st.output.clone())
            .unwrap_or_else(|| "Aucune carte son".into());
        let label = if ok {
            format!(
                "{}  ·  {}  ·  {} kHz  ·  {} in / {} out",
                st.host,
                short(&dev, 28),
                fmt_khz(st.sample_rate),
                st.in_channels,
                st.out_channels
            )
        } else {
            dev
        };
        let font = t::medium(12.0);
        let gal = ui.painter().layout_no_wrap(label, font, t::TEXT_2);
        let pw = gal.size().x + 44.0;
        let pr = egui::Rect::from_min_size(
            egui::pos2(full.right() - pw - 8.0, full.center().y - 15.0),
            vec2(pw, 30.0),
        );
        let resp = ui.interact(
            pr,
            ui.make_persistent_id("device_pill"),
            egui::Sense::click(),
        );
        let p = ui.painter();
        p.rect_filled(
            pr,
            15.0,
            if resp.hovered() {
                t::SURFACE_3
            } else {
                t::SURFACE_2
            },
        );
        p.circle_filled(
            egui::pos2(pr.left() + 16.0, pr.center().y),
            4.0,
            if ok { t::PLAY } else { t::REC },
        );
        p.galley(
            egui::pos2(pr.left() + 28.0, pr.center().y - gal.size().y / 2.0),
            gal,
            t::TEXT_2,
        );
        if resp.on_hover_text("Réglages de la carte son").clicked() {
            self.open_audio();
        }
    }

    fn main_menu(&mut self, ui: &mut egui::Ui) {
        let stopped = self.is_stopped();
        ui.label(
            egui::RichText::new("SESSION")
                .font(t::semibold(10.5))
                .color(t::TEXT_3),
        );
        if ui
            .add_enabled(stopped, egui::Button::new("Nouvelle session…"))
            .clicked()
        {
            self.open_new_session();
        }
        if ui
            .add_enabled(stopped, egui::Button::new("Ouvrir…"))
            .clicked()
        {
            self.open_session();
        }
        if ui.button("Enregistrer  (Ctrl+S)").clicked() {
            self.save();
        }
        if ui
            .add_enabled(stopped, egui::Button::new("Enregistrer sous…"))
            .clicked()
        {
            self.save_as();
        }
        if ui.button("Afficher le dossier").clicked() {
            let _ = self.project.ensure_dirs();
            open_in_file_manager(&self.project.dir);
        }
        if ui
            .add_enabled(
                self.project.has_audio() && stopped,
                egui::Button::new("Exporter…  (Ctrl+E)"),
            )
            .clicked()
        {
            self.open_export();
        }
        ui.separator();
        ui.label(
            egui::RichText::new("ENREGISTREMENT")
                .font(t::semibold(10.5))
                .color(t::TEXT_3),
        );
        for f in [RecFormat::Int24, RecFormat::Float32] {
            if ui
                .add_enabled(
                    stopped,
                    egui::RadioButton::new(
                        self.project.rec_format == f,
                        format!("WAV {}", f.label()),
                    ),
                )
                .clicked()
            {
                self.project.rec_format = f;
                self.project.dirty = true;
            }
        }
        ui.separator();
        ui.label(
            egui::RichText::new("SOUNDBOARD")
                .font(t::semibold(10.5))
                .color(t::TEXT_3),
        );
        if ui.button("Ouvrir un soundboard…").clicked()
            && let Some(p) = rfd::FileDialog::new()
                .add_filter("Soundboard", &["json"])
                .pick_file()
        {
            match Board::load(&p) {
                Ok(b) => {
                    self.engine.pad(PadCmd::StopAll { fade: 0 });
                    self.board = b;
                    self.board_path = p;
                    self.sb.selected = None;
                    self.sb.page = 0;
                    let paths: Vec<PathBuf> =
                        self.board.all_pads().map(|(_, p)| p.path.clone()).collect();
                    for p in paths {
                        self.cache.request(&p, None);
                    }
                    let port = self.board.midi_port.clone();
                    self.midi.connect(port.as_deref());
                    self.sync_engine_to_board();
                }
                Err(e) => self.notify(format!("{e:#}"), true),
            }
        }
        if ui.button("Enregistrer le soundboard sous…").clicked()
            && let Some(p) = rfd::FileDialog::new()
                .add_filter("Soundboard", &["json"])
                .set_file_name("soundboard.json")
                .save_file()
        {
            self.board_path = p;
            self.board_changed = Some(self.now);
            self.save_board_now();
        }
        ui.separator();
        if ui.button("Réglages audio…").clicked() {
            self.open_audio();
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 16.0;
            let dim = |s: String| {
                egui::RichText::new(s)
                    .font(t::regular(11.5))
                    .color(t::TEXT_3)
            };
            ui.label(dim(format!(
                "Session {} kHz · WAV {} · buffer {}",
                fmt_khz(self.project.sample_rate),
                self.project.rec_format.label(),
                self.engine
                    .status
                    .buffer
                    .map_or("auto".to_string(), |b| b.to_string())
            )));
            let sh = &self.engine.shared;
            let xr = sh.xruns.load(Ordering::Relaxed) + sh.underruns.load(Ordering::Relaxed);
            let ov = sh.overruns.load(Ordering::Relaxed);
            if xr > 0 || ov > 0 {
                let r = ui.add(
                    egui::Label::new(
                        egui::RichText::new(format!("⚠ {xr} décrochages · {ov} pertes disque"))
                            .font(t::medium(11.5))
                            .color(t::ORANGE),
                    )
                    .sense(egui::Sense::click()),
                );
                if r.on_hover_text("Cliquer pour remettre à zéro").clicked() {
                    sh.xruns.store(0, Ordering::Relaxed);
                    sh.underruns.store(0, Ordering::Relaxed);
                    sh.overruns.store(0, Ordering::Relaxed);
                }
            }
            if let Some(e) = sh.last_error.lock().clone() {
                ui.label(
                    egui::RichText::new(format!("⚠ {e}"))
                        .font(t::medium(11.5))
                        .color(t::REC),
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
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
                    let col = if secs < 1800 { t::REC } else { t::TEXT_3 };
                    ui.label(
                        egui::RichText::new(format!(
                            "Disque {:.0} Go · ≈ {} d'enregistrement ({} piste{})",
                            free as f64 / 1e9,
                            fmt_duration(secs),
                            armed,
                            if armed > 1 { "s" } else { "" }
                        ))
                        .font(t::regular(11.5))
                        .color(col),
                    );
                }
            });
        });
    }

    // ---------------------------------------------------------------- dialogs

    fn dialog_header(ui: &mut egui::Ui, title: &str) -> bool {
        let mut close = false;
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(title).font(t::semibold(17.0)));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                close = w::icon_button(
                    ui,
                    Icon::Close,
                    sq(26.0),
                    BtnStyle::Ghost,
                    true,
                    "Fermer (Échap)",
                )
                .clicked();
            });
        });
        ui.add_space(10.0);
        close
    }

    fn section(ui: &mut egui::Ui, title: &str) {
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(title.to_uppercase())
                .font(t::semibold(10.5))
                .color(t::TEXT_3),
        );
        ui.add_space(2.0);
    }

    fn audio_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dlg) = self.audio_dlg.take() else {
            return;
        };
        let mut close = false;
        let mut apply = false;
        egui::Modal::new(egui::Id::unique("audio_dlg"))
            .frame(t::window_frame())
            .show(ctx, |ui| {
                ui.set_width(480.0);
                close = Self::dialog_header(ui, "Carte son");
                egui::Grid::new("audio_grid").num_columns(2).spacing([16.0, 10.0]).show(ui, |ui| {
                    let combo_w = 320.0;
                    ui.label(egui::RichText::new("Pilote").color(t::TEXT_2));
                    let cur = dlg.prefs.host.clone().unwrap_or_default();
                    egui::ComboBox::from_id_salt("host").width(combo_w).selected_text(&cur).show_ui(ui, |ui| {
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

                    ui.label(egui::RichText::new("Entrée").color(t::TEXT_2));
                    let cur = dlg.prefs.input.clone().unwrap_or_else(|| "Par défaut".into());
                    egui::ComboBox::from_id_salt("in").width(combo_w).selected_text(&cur).show_ui(ui, |ui| {
                        if ui.selectable_label(dlg.prefs.input.is_none(), "Par défaut").clicked() {
                            dlg.prefs.input = None;
                        }
                        for d in &dlg.ins {
                            let txt = format!("{}   {} canaux", d.name, d.channels);
                            if ui.selectable_label(dlg.prefs.input.as_deref() == Some(&d.name), txt).clicked() {
                                dlg.prefs.input = Some(d.name.clone());
                                if dlg.outs.iter().any(|o| o.name == d.name) {
                                    dlg.prefs.output = Some(d.name.clone());
                                }
                            }
                        }
                    });
                    ui.end_row();

                    ui.label(egui::RichText::new("Sortie").color(t::TEXT_2));
                    let cur = dlg.prefs.output.clone().unwrap_or_else(|| "Par défaut".into());
                    egui::ComboBox::from_id_salt("out").width(combo_w).selected_text(&cur).show_ui(ui, |ui| {
                        if ui.selectable_label(dlg.prefs.output.is_none(), "Par défaut").clicked() {
                            dlg.prefs.output = None;
                        }
                        for d in &dlg.outs {
                            let txt = format!("{}   {} canaux", d.name, d.channels);
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
                    ui.label(egui::RichText::new("Écoute multipiste").color(t::TEXT_2));
                    let l = dlg.prefs.out_left;
                    egui::ComboBox::from_id_salt("outpair")
                        .width(140.0)
                        .selected_text(format!("Sorties {} – {}", l + 1, l + 2))
                        .height(300.0)
                        .show_ui(ui, |ui| {
                            let mut i = 0;
                            while i < out_ch.max(2) {
                                if ui.selectable_label(l == i, format!("Sorties {} – {}", i + 1, i + 2)).clicked() {
                                    dlg.prefs.out_left = i;
                                }
                                i += 2;
                            }
                        });
                    ui.end_row();

                    let rates: Vec<u32> = dlg
                        .ins
                        .iter()
                        .find(|d| Some(&d.name) == dlg.prefs.input.as_ref())
                        .or(dlg.ins.first())
                        .map(|d| d.rates.clone())
                        .filter(|r| !r.is_empty())
                        .unwrap_or_else(|| audio::COMMON_RATES.to_vec());
                    ui.label(egui::RichText::new("Fréquence").color(t::TEXT_2));
                    let cur = dlg.prefs.sample_rate.map_or("Par défaut".to_string(), |r| format!("{} kHz", fmt_khz(r)));
                    egui::ComboBox::from_id_salt("sr").width(140.0).selected_text(cur).show_ui(ui, |ui| {
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

                    ui.label(egui::RichText::new("Buffer").color(t::TEXT_2));
                    let cur = dlg.prefs.buffer.map_or("Pilote".to_string(), |b| format!("{b} échantillons"));
                    egui::ComboBox::from_id_salt("buf").width(140.0).selected_text(cur).show_ui(ui, |ui| {
                        if ui.selectable_label(dlg.prefs.buffer.is_none(), "Pilote").clicked() {
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
                ui.add_space(10.0);
                egui::Frame::new().fill(t::SURFACE_3).corner_radius(t::R).inner_margin(12).show(ui, |ui| {
                    ui.label(
                        egui::RichText::new(
                            "Dante — Windows : pilote ASIO puis « Dante Virtual Soundcard ». macOS : CoreAudio puis « Dante Virtual Soundcard ». \
                             La fréquence doit correspondre à Dante Controller. Le réglage est mémorisé.",
                        )
                        .font(t::regular(12.0))
                        .color(t::TEXT_2),
                    );
                });
                if let Some(e) = &dlg.error {
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new(e).font(t::medium(12.5)).color(t::REC));
                }
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    if ui.button("Actualiser").clicked() {
                        dlg.refresh();
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if w::pill(ui, None, "Appliquer", Some(t::ACCENT), true).clicked() {
                            apply = true;
                        }
                    });
                });
            });
        if apply {
            match self.apply_audio(dlg.prefs.clone()) {
                Ok(()) => {
                    let st = &self.engine.status;
                    let msg = format!(
                        "{} · {} kHz · {} entrées",
                        st.input.clone().unwrap_or_else(|| "Pas d'entrée".into()),
                        fmt_khz(st.sample_rate),
                        st.in_channels
                    );
                    self.notify(msg, false);
                    return;
                }
                Err(e) => dlg.error = Some(e),
            }
        }
        if !close {
            self.audio_dlg = Some(dlg);
        }
    }

    fn export_dialog(&mut self, ctx: &egui::Context) {
        if let Some(job) = &self.export_job {
            let p = job.progress.load(Ordering::Relaxed) as f32 / 1000.0;
            let mut cancel = false;
            egui::Modal::new(egui::Id::unique("export_job"))
                .frame(t::window_frame())
                .show(ctx, |ui| {
                    ui.set_width(360.0);
                    ui.label(egui::RichText::new("Export en cours").font(t::semibold(16.0)));
                    ui.add_space(12.0);
                    ui.add(
                        egui::ProgressBar::new(p)
                            .desired_width(360.0)
                            .desired_height(8.0)
                            .fill(t::ACCENT),
                    );
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(format!("{:.0} %", p * 100.0)).color(t::TEXT_2),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("Annuler").clicked() {
                                cancel = true;
                            }
                        });
                    });
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
                            format!("Export terminé · {} fichier(s)", files.len()),
                            false,
                        );
                        if let Some(d) = dir {
                            open_in_file_manager(&d);
                        }
                    }
                    Err(e) => self.notify(format!("{e:#}"), true),
                }
            }
            return;
        }
        let Some(mut d) = self.export_dlg.take() else {
            return;
        };
        let mut close = false;
        let mut go = false;
        let sr = self.project.sample_rate.max(1);
        egui::Modal::new(egui::Id::unique("export_dlg"))
            .frame(t::window_frame())
            .show(ctx, |ui| {
                ui.set_width(560.0);
                close = Self::dialog_header(ui, "Exporter");

                Self::section(ui, "Pistes");
                let with_audio: Vec<(u64, String, usize)> = self
                    .project
                    .tracks
                    .iter()
                    .enumerate()
                    .filter(|(_, t)| !t.clips.is_empty())
                    .map(|(i, t)| (t.id, t.name.clone(), i))
                    .collect();
                ui.horizontal(|ui| {
                    if ui.small_button("Toutes").clicked() {
                        d.tracks = with_audio.iter().map(|x| x.0).collect();
                    }
                    if ui.small_button("Aucune").clicked() {
                        d.tracks.clear();
                    }
                    if !self.selected_tracks.is_empty()
                        && ui.small_button("Sélection de la timeline").clicked()
                    {
                        d.tracks = with_audio
                            .iter()
                            .map(|x| x.0)
                            .filter(|id| self.selected_tracks.contains(id))
                            .collect();
                    }
                    ui.label(
                        egui::RichText::new(format!(
                            "{} / {} sélectionnée(s)",
                            d.tracks.len(),
                            with_audio.len()
                        ))
                        .font(t::regular(12.0))
                        .color(t::TEXT_3),
                    );
                });
                egui::Frame::new()
                    .fill(t::SURFACE)
                    .corner_radius(t::R)
                    .inner_margin(8)
                    .show(ui, |ui| {
                        egui::ScrollArea::vertical()
                            .max_height(150.0)
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                egui::Grid::new("exp_tracks")
                                    .num_columns(3)
                                    .spacing([18.0, 4.0])
                                    .show(ui, |ui| {
                                        for (k, (id, name, idx)) in with_audio.iter().enumerate() {
                                            let mut on = d.tracks.contains(id);
                                            let txt = egui::RichText::new(format!(
                                                "{:02}  {}",
                                                idx + 1,
                                                short(name, 18)
                                            ));
                                            if ui.checkbox(&mut on, txt).changed() {
                                                if on {
                                                    d.tracks.insert(*id);
                                                } else {
                                                    d.tracks.remove(id);
                                                }
                                            }
                                            if k % 3 == 2 {
                                                ui.end_row();
                                            }
                                        }
                                    });
                            });
                    });

                Self::section(ui, "Plage");
                ui.horizontal(|ui| {
                    ui.radio_value(
                        &mut d.range,
                        RangeChoice::All,
                        format!(
                            "Toute la session ({})",
                            fmt_duration(self.project.end() / sr as u64)
                        ),
                    );
                    let sel_txt = match self.selection {
                        Some((a, b)) => format!(
                            "Sélection {} → {}",
                            fmt_time_short(a, sr),
                            fmt_time_short(b, sr)
                        ),
                        None => "Sélection (glisser sur la règle)".into(),
                    };
                    ui.add_enabled_ui(self.selection.is_some(), |ui| {
                        ui.radio_value(&mut d.range, RangeChoice::Selection, sel_txt);
                    });
                });

                Self::section(ui, "Contenu");
                ui.horizontal(|ui| {
                    ui.checkbox(&mut d.s.stems, "Pistes séparées (WAV mono)");
                    ui.checkbox(&mut d.s.mix, "Mix stéréo des pistes");
                });

                Self::section(ui, "Format");
                ui.horizontal(|ui| {
                    for r in [44100, 48000, 88200, 96000] {
                        ui.selectable_value(&mut d.s.sample_rate, r, format!("{} kHz", fmt_khz(r)));
                    }
                });
                ui.horizontal(|ui| {
                    for b in BitDepth::ALL {
                        ui.selectable_value(&mut d.s.depth, b, b.label());
                    }
                });
                ui.add_enabled(
                    d.s.depth == BitDepth::I16,
                    egui::Checkbox::new(&mut d.s.dither, "Dither (recommandé en 16 bits)"),
                );

                Self::section(ui, "Destination");
                ui.horizontal(|ui| {
                    let mut txt = d.s.dir.display().to_string();
                    if ui
                        .add(egui::TextEdit::singleline(&mut txt).desired_width(470.0))
                        .changed()
                    {
                        d.s.dir = PathBuf::from(txt);
                    }
                    if ui.button("…").clicked()
                        && let Some(p) = rfd::FileDialog::new()
                            .set_directory(&self.project.dir)
                            .pick_folder()
                    {
                        d.s.dir = p;
                    }
                });
                ui.add_space(14.0);
                let n_files = if d.s.stems { d.tracks.len() } else { 0 }
                    + usize::from(d.s.mix && !d.tracks.is_empty());
                ui.horizontal(|ui| {
                    let conv = if d.s.sample_rate != sr {
                        " · conversion de fréquence"
                    } else {
                        ""
                    };
                    ui.label(
                        egui::RichText::new(format!("{n_files} fichier(s){conv}")).color(t::TEXT_3),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if w::pill(
                            ui,
                            Some(Icon::Export),
                            "Exporter",
                            Some(t::ACCENT),
                            n_files > 0,
                        )
                        .clicked()
                        {
                            go = true;
                        }
                    });
                });
            });
        if go {
            self.stop();
            let mut s = d.s.clone();
            s.tracks = d.tracks.iter().copied().collect();
            s.range = match d.range {
                RangeChoice::Selection => self.selection,
                RangeChoice::All => None,
            };
            self.export_last = Some(ExportSettings {
                tracks: Vec::new(),
                range: None,
                ..s.clone()
            });
            self.export_job = Some(ExportJob::start(&self.project, s));
            return;
        }
        if !close {
            self.export_dlg = Some(d);
        }
    }

    fn new_session_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut d) = self.new_dlg.take() else {
            return;
        };
        let mut close = false;
        let mut create = false;
        egui::Modal::new(egui::Id::unique("new_dlg"))
            .frame(t::window_frame())
            .show(ctx, |ui| {
                ui.set_width(460.0);
                close = Self::dialog_header(ui, "Nouvelle session");
                Self::section(ui, "Nom");
                ui.add(egui::TextEdit::singleline(&mut d.name).desired_width(460.0));
                Self::section(ui, "Emplacement");
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(short(&d.parent.display().to_string(), 52))
                            .color(t::TEXT_2),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Choisir…").clicked()
                            && let Some(p) = rfd::FileDialog::new()
                                .set_directory(&d.parent)
                                .pick_folder()
                        {
                            d.parent = p;
                        }
                    });
                });
                ui.add_space(6.0);
                ui.checkbox(
                    &mut d.keep_tracks,
                    "Reprendre les pistes et les entrées actuelles",
                );
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if w::pill(
                            ui,
                            None,
                            "Créer",
                            Some(t::ACCENT),
                            !d.name.trim().is_empty(),
                        )
                        .clicked()
                        {
                            create = true;
                        }
                    });
                });
            });
        if create {
            self.create_session(d);
            return;
        }
        if !close {
            self.new_dlg = Some(d);
        }
    }

    fn toast_ui(&mut self, ctx: &egui::Context) {
        let Some(tt) = &self.toast else {
            return;
        };
        if self.now > tt.until {
            self.toast = None;
            return;
        }
        let fade = ((tt.until - self.now) / 0.3).min(1.0) as f32;
        egui::Area::new(egui::Id::unique("toast"))
            .anchor(Align2::CENTER_BOTTOM, vec2(0.0, -48.0))
            .order(egui::Order::Tooltip)
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(t::SURFACE_3.gamma_multiply(fade))
                    .stroke(egui::Stroke::new(1.0, t::BORDER))
                    .corner_radius(20)
                    .inner_margin(egui::Margin::symmetric(18, 10))
                    .shadow(egui::Shadow {
                        offset: [0, 6],
                        blur: 24,
                        spread: 0,
                        color: Color32::from_black_alpha((110.0 * fade) as u8),
                    })
                    .show(ui, |ui| {
                        ui.set_max_width(620.0);
                        ui.horizontal(|ui| {
                            let (r, _) =
                                ui.allocate_exact_size(Vec2::splat(8.0), egui::Sense::hover());
                            ui.painter().circle_filled(
                                r.center(),
                                4.0,
                                if tt.error { t::REC } else { t::PLAY },
                            );
                            ui.label(
                                egui::RichText::new(&tt.text)
                                    .font(t::medium(13.0))
                                    .color(t::TEXT.gamma_multiply(fade)),
                            );
                        });
                    });
            });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.now = ctx.input(|i| i.time);
        self.engine.collect_garbage();
        if self.cache.poll() {
            self.sb.midi_dirty = true;
        }
        self.update_playhead();
        self.update_meters(&ctx);
        self.global_keys(&ctx);
        self.update_title(&ctx);
        if let Some(tch) = self.board_changed
            && self.now - tch > 1.0
        {
            self.save_board_now();
        }
        self.soundboard_background(&ctx);

        egui::Panel::top("top")
            .exact_size(56.0)
            .frame(
                egui::Frame::new()
                    .fill(t::SURFACE)
                    .inner_margin(egui::Margin::symmetric(10, 0)),
            )
            .show(ui, |ui| self.top_bar(ui));
        egui::Panel::bottom("status")
            .exact_size(26.0)
            .frame(
                egui::Frame::new()
                    .fill(t::SURFACE)
                    .inner_margin(egui::Margin::symmetric(14, 0)),
            )
            .show(ui, |ui| self.status_bar(ui));

        if self.tab == 0 {
            self.multitrack_ui(ui);
        } else {
            self.soundboard_ui(ui);
        }

        self.audio_dialog(&ctx);
        self.export_dialog(&ctx);
        self.new_session_dialog(&ctx);
        self.toast_ui(&ctx);

        ctx.request_repaint_after(std::time::Duration::from_millis(16));
    }

    fn on_exit(&mut self) {
        self.stop();
        self.autosave();
        self.board_changed = Some(0.0);
        self.save_board_now();
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

pub fn fmt_time_short(samples: u64, sr: u32) -> String {
    let secs = samples / sr.max(1) as u64;
    let (h, m, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

pub fn fmt_khz(sr: u32) -> String {
    if sr.is_multiple_of(1000) {
        format!("{}", sr / 1000)
    } else {
        format!("{:.1}", sr as f64 / 1000.0)
    }
}

pub fn fmt_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs / 60) % 60;
    if h > 0 {
        format!("{h} h {m:02}")
    } else {
        format!("{m} min {:02}", secs % 60)
    }
}

pub fn short(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut x: String = s.chars().take(n.saturating_sub(1)).collect();
        x.push('…');
        x
    }
}

pub fn open_in_file_manager(path: &Path) {
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
        assert_eq!(fmt_time_short(48000 * 3725, 48000), "1:02:05");
        assert_eq!(short("abcdef", 4), "abc…");
    }
}
