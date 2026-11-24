//! Application : état global, transport, sessions, barre supérieure et fenêtres.
//! Les onglets sont dans `ui::multitrack` et `ui::soundboard`.

use crate::audio::{self, AudioPrefs, DeviceInfo, Engine, RecTarget, Shared};
use crate::export::{BitDepth, ExportJob, ExportSettings};
use crate::midi::Midi;
use crate::player::Player;
use crate::project::{self, Clip, PROJECT_EXT, Project, RecFormat};
use crate::recorder::{FinishError, RecResult, RecSpec, Recorder};
use crate::recovery;
use crate::render::{self, PeakData, Peaks};
use crate::sampler::PadCmd;
use crate::soundboard::{Board, SampleCache, db_to_gain};
use crate::ui::theme as t;
use crate::ui::widgets::{self as w, BtnStyle, Icon, MeterState};
use eframe::egui::{self, Align2, Color32, Key, Rect, vec2};
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
    /// Périphériques utilisés quand l'entrée / la sortie est « Par défaut ».
    pub default_in: Option<String>,
    pub default_out: Option<String>,
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
            default_in: None,
            default_out: None,
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
        (self.default_in, self.default_out) =
            audio::default_device_names(self.prefs.host.as_deref());
        self.listed_host = self.prefs.host.clone();
    }

    /// Entrée choisie (ou celle par défaut du pilote).
    fn input_info(&self) -> Option<&DeviceInfo> {
        let name = self.prefs.input.as_ref().or(self.default_in.as_ref())?;
        self.ins.iter().find(|d| &d.name == name)
    }

    /// Sortie choisie (ou celle par défaut du pilote).
    fn output_info(&self) -> Option<&DeviceInfo> {
        let name = self.prefs.output.as_ref().or(self.default_out.as_ref())?;
        self.outs.iter().find(|d| &d.name == name)
    }

    /// Fréquences possibles : communes à l'entrée et à la sortie.
    fn rates(&self) -> Vec<u32> {
        let ins = self
            .input_info()
            .map(|d| d.rates.clone())
            .unwrap_or_default();
        let outs = self
            .output_info()
            .map(|d| d.rates.clone())
            .unwrap_or_default();
        let both: Vec<u32> = ins.iter().copied().filter(|r| outs.contains(r)).collect();
        if !both.is_empty() {
            both
        } else if !ins.is_empty() {
            ins
        } else {
            outs
        }
    }

    /// Fréquence retenue en « Auto ».
    fn default_rate(&self) -> Option<u32> {
        self.input_info()
            .and_then(|d| d.default_rate)
            .or_else(|| self.output_info().and_then(|d| d.default_rate))
    }

    /// Tailles de buffer acceptées par l'entrée et la sortie (trames), si connues.
    fn buffer_range(&self) -> Option<(u32, u32)> {
        let a = self.input_info().and_then(|d| d.buffer);
        let b = self.output_info().and_then(|d| d.buffer);
        match (a, b) {
            (Some((a0, a1)), Some((b0, b1))) if a0.max(b0) <= a1.min(b1) => {
                Some((a0.max(b0), a1.min(b1)))
            }
            (Some(r), _) | (None, Some(r)) => Some(r),
            _ => None,
        }
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
    /// Poignée de fondu survolée dans l'éditeur (0 = entrée, 1 = sortie).
    pub hover_fade: Option<usize>,
    pub midi_seen: f64,
    pub midi_last: Option<String>,
    pub midi_dirty: bool,
    pub flash: HashMap<u16, f64>,
    /// Mode « déplacer » : les pads se glissent au lieu de se déclencher.
    pub move_mode: bool,
    /// Pad en cours de déplacement.
    pub drag_pad: Option<u16>,
    /// Zones des onglets de page (cibles de dépôt d'un pad).
    pub page_tabs: Vec<Rect>,
    /// Page dont la suppression attend confirmation.
    pub confirm_delete_page: Option<usize>,
    /// Fenêtre « Réglages du soundboard » ouverte.
    pub settings_open: bool,
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
    /// Périphériques de sortie du pilote en cours (choix des destinations d'écoute).
    pub out_devices: Vec<DeviceInfo>,
    /// Dernier choix mono / stéréo du menu « Ajouter une piste ».
    pub add_stereo: bool,
    /// Repère dont le menu (renommer / supprimer) est ouvert.
    pub marker_editing: Option<u64>,
    /// Piste en cours de renommage (champ de saisie à la place du nom).
    pub renaming_track: Option<u64>,
    /// Donner le focus au champ de renommage à la prochaine image.
    pub renaming_focus: bool,
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
    /// Instant jusqu'auquel un second appui clavier arrête l'enregistrement.
    pub stop_armed_until: f64,
    /// Fenêtre de confirmation de fermeture pendant un enregistrement.
    pub confirm_close: bool,
    /// Suivi de la progression de l'enregistrement (détection de perte de signal).
    pub rec_watch: (u64, f64),
    /// Dernière sauvegarde automatique.
    pub last_autosave: f64,
    /// Empêche la mise en veille de l'ordinateur pendant un enregistrement.
    pub awake: Option<keepawake::KeepAwake>,
    // Soundboard
    pub board: Board,
    pub board_path: PathBuf,
    pub board_changed: Option<f64>,
    pub cache: SampleCache,
    pub midi: Midi,
    pub midi_ports: Vec<String>,
    pub sb: SbState,
    // Montage
    pub undo: Vec<Snapshot>,
    pub redo: Vec<Snapshot>,
    pub sel_clips: HashSet<u64>,
    pub snap: bool,
    pub clip_drag: Option<crate::ui::multitrack::ClipDrag>,
}

/// État restaurable par Annuler / Rétablir.
#[derive(Clone)]
pub struct Snapshot {
    tracks: Vec<project::Track>,
    markers: Vec<project::Marker>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        t::apply(&cc.egui_ctx);
        let prefs = AudioPrefs::load();
        let board = Board::load_default();
        let (engine, err) = match Engine::open(&prefs, &board.pad_out()) {
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
            add_stereo: false,
            out_devices: Vec::new(),
            marker_editing: None,
            renaming_track: None,
            renaming_focus: false,
            master_play: Default::default(),
            master_pad: Default::default(),
            view: View {
                spp: 1200.0,
                scroll: 0.0,
                track_h: 62.0,
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
            stop_armed_until: 0.0,
            confirm_close: false,
            rec_watch: (0, 0.0),
            last_autosave: 0.0,
            awake: None,
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
            undo: Vec::new(),
            redo: Vec::new(),
            sel_clips: HashSet::new(),
            snap: true,
            clip_drag: None,
        };
        app.midi.shared.lock().ctx = Some(cc.egui_ctx.clone());
        let port = app.board.midi_port.clone();
        app.midi.connect(port.as_deref());
        let led_port = app.board.led_port.clone();
        app.midi.connect_leds(led_port.as_deref());
        app.out_devices = audio::list_devices(app.prefs.host.as_deref()).1;
        app.sync_engine_to_board();
        let paths: Vec<PathBuf> = app.board.all_pads().map(|(_, p)| p.path.clone()).collect();
        for p in paths {
            app.cache.request(&p, Some(&cc.egui_ctx));
        }
        // Enregistrement interrompu lors de la dernière utilisation : on restaure.
        if let Some(path) = recovery::interrupted_session() {
            app.load_session(&path);
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
        let out_ch = self.engine.status.pad_out_channels.max(1) as usize;
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
        // (piste, nom, premier canal, stéréo)
        let armed: Vec<(u64, String, u16, bool)> = self
            .project
            .tracks
            .iter()
            .filter(|t| t.armed && t.input_fits(in_ch))
            .filter_map(|t| t.input.map(|i| (t.id, t.name.clone(), i, t.stereo)))
            .collect();
        // Pistes armées dont l'entrée n'existe pas sur la carte (ex. Dante revenue avec
        // moins de canaux) : on enregistre les autres, mais on le signale clairement.
        let skipped: Vec<String> = self
            .project
            .tracks
            .iter()
            .filter(|t| t.armed && !t.input_fits(in_ch))
            .map(|t| t.name.clone())
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
        // Journal d'une prise précédente non intégrée (sauvegarde échouée) : on la
        // récupère avant d'écrire le nouveau journal, pour ne rien perdre.
        if recovery::journal_path(&self.project.dir).exists()
            && let Ok(n) = recovery::recover(&mut self.project)
            && n > 0
        {
            self.load_peaks();
        }

        let mut specs = Vec::new();
        let mut targets = Vec::new();
        let mut used = HashSet::new();
        self.live.clear();
        for (track_id, name, input, stereo) in armed {
            // Nom unique sur le disque et dans cette prise, sans tenir compte de la casse.
            let file = self.project.new_take_file_avoiding(&name, &used);
            used.insert(file.to_lowercase());
            let width: u16 = if stereo { 2 } else { 1 };
            // 4 s de tampon par piste, quel que soit le nombre de canaux.
            let (prod, cons) = rtrb::RingBuffer::new(sr as usize * 4 * width as usize);
            let peaks: Peaks = Arc::new(RwLock::new(PeakData::default()));
            specs.push(RecSpec {
                track_id,
                path: self.project.audio_dir().join(&file),
                file,
                peaks: peaks.clone(),
                cons,
                channels: width,
            });
            targets.push(if stereo {
                RecTarget::stereo(input as usize, prod)
            } else {
                RecTarget::new(input as usize, prod)
            });
            self.live.push(LiveTake { track_id, peaks });
        }
        // Sécurité : la session et le journal des prises sont écrits AVANT de lancer
        // l'enregistrement, pour pouvoir tout retrouver après un crash.
        let journal = recovery::Journal {
            start: self.playhead,
            sample_rate: sr,
            takes: specs
                .iter()
                .map(|s| recovery::JournalTake {
                    track_id: s.track_id,
                    file: s.file.clone(),
                })
                .collect(),
        };
        let _ = self.project.save();
        recovery::remember_session(&self.project);
        if let Err(e) = recovery::write_journal(&self.project.dir, &journal) {
            self.notify(
                format!("Attention : journal de secours non écrit ({e:#})"),
                true,
            );
        }
        let recorder = match Recorder::start(specs, sr, self.project.rec_format) {
            Ok(r) => r,
            Err(e) => {
                recovery::remove_journal(&self.project.dir);
                self.live.clear();
                self.notify(
                    format!("Impossible de démarrer l'écriture disque : {e}"),
                    true,
                );
                return;
            }
        };
        if let Err(e) = self.engine.start_record(targets) {
            recovery::remove_journal(&self.project.dir);
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
        // Pas de mise en veille pendant la prise (sans effet si le système refuse).
        self.awake = keepawake::Builder::default()
            .idle(true)
            .sleep(true)
            .reason("Enregistrement multipiste en cours")
            .app_name("TracksPlus")
            .app_reverse_domain("app.tracksplus")
            .create()
            .ok();
        self.transport = Transport::Recording {
            start: self.playhead,
        };
        self.view.nav_until = 0.0;
        self.reveal_recording_tracks();
        // Une prise modifie la timeline : l'historique d'annulation d'avant ne s'applique
        // plus (sinon Ctrl+Z pourrait retirer la prise de la session).
        self.undo.clear();
        self.redo.clear();
        if !skipped.is_empty() {
            self.notify(
                format!(
                    "Attention : {} piste(s) armée(s) sans entrée valide ne sont PAS enregistrées : {}",
                    skipped.len(),
                    skipped.join(", ")
                ),
                true,
            );
        }
    }

    pub fn stop(&mut self) {
        match self.transport {
            Transport::Stopped => {}
            Transport::Playing { .. } => {
                self.engine.stop_playback();
                self.player = None;
            }
            Transport::Recording { start } => {
                // 1) Le callback rend les cibles ; leurs producteurs sont libérés ici.
                let released = self
                    .engine
                    .stop_record(std::time::Duration::from_millis(1500));
                self.engine.stop_playback();
                self.player = None;
                // 2) Le thread disque écrit tout ce qui reste puis ferme les fichiers.
                let finished = self.recorder.take().map(Recorder::finish);
                self.awake = None;
                self.live_takes_to_clips(start, finished, released);
            }
        }
        self.transport = Transport::Stopped;
        self.engine.shared.counting.store(false, Ordering::Relaxed);
    }

    /// Ajoute les prises terminées à la session, la sauvegarde et nettoie le journal.
    fn live_takes_to_clips(
        &mut self,
        start: u64,
        finished: Option<Result<Vec<RecResult>, FinishError>>,
        released: bool,
    ) {
        let results = match finished {
            Some(Ok(r)) => r,
            Some(Err(FinishError::Timeout)) => {
                // Le disque ne répond plus : surtout ne pas bloquer l'interface. Le journal est
                // conservé ; les prises seront réparées et réintégrées à la prochaine ouverture.
                self.live.clear();
                self.engine.shared.overruns.store(0, Ordering::Relaxed);
                self.notify(
                    "Le disque ne répond plus : les prises seront récupérées automatiquement à la prochaine ouverture de la session",
                    true,
                );
                return;
            }
            _ => {
                // Thread disque en échec : on récupère depuis les fichiers grâce au journal.
                self.live.clear();
                self.engine.shared.overruns.store(0, Ordering::Relaxed);
                let n = recovery::recover(&mut self.project).unwrap_or(0);
                self.load_peaks();
                self.notify(
                    format!(
                        "Problème d'écriture disque : {n} prise(s) récupérée(s) depuis le disque"
                    ),
                    true,
                );
                return;
            }
        };
        let mut errors = Vec::new();
        let mut max_len = 0;
        for r in results {
            if let Some(e) = &r.error {
                errors.push(e.clone());
            }
            let peaks = self
                .live
                .iter()
                .find(|l| l.track_id == r.track_id)
                .map(|l| l.peaks.clone());
            // Une prise longue peut être répartie sur plusieurs fichiers : une prise par
            // fichier, posées bout à bout.
            let mut pos = start;
            for seg in &r.segments {
                if seg.frames == 0 {
                    let _ = std::fs::remove_file(self.project.audio_dir().join(&seg.file));
                    continue;
                }
                let id = self.project.alloc_id();
                if let Some(p) = &peaks {
                    // La forme d'onde en direct couvre toute la prise ; chaque fichier en a
                    // une à lui, calculée à part pour rester exacte.
                    if r.segments.len() == 1 {
                        self.peaks.insert(id, p.clone());
                    } else {
                        let pk: Peaks = Arc::new(RwLock::new(PeakData::default()));
                        let path = self.project.audio_dir().join(&seg.file);
                        let pk2 = pk.clone();
                        std::thread::spawn(move || render::compute_peaks(&path, &pk2));
                        self.peaks.insert(id, pk);
                    }
                }
                if let Some(tr) = self.project.track_mut(r.track_id) {
                    tr.clips.push(Clip {
                        id,
                        file: seg.file.clone(),
                        start: pos,
                        offset: 0,
                        len: seg.frames,
                        src_len: seg.frames,
                        fade_in: 0,
                        fade_out: 0,
                    });
                }
                pos += seg.frames;
            }
            max_len = max_len.max(r.frames());
        }
        self.live.clear();
        self.playhead = start + max_len;
        self.project.dirty = true;
        let save = self.project.save();
        if save.is_ok() {
            // Les prises sont référencées dans la session : le journal n'est plus utile.
            recovery::remove_journal(&self.project.dir);
        }
        let overruns = self.engine.shared.overruns.swap(0, Ordering::Relaxed);
        // Décrochages du pilote pendant la prise (ils restent affichés en bas d'écran).
        let xruns = self.engine.shared.xruns.load(Ordering::Relaxed);
        if !errors.is_empty() {
            self.notify(
                format!("Erreur d'enregistrement : {}", errors.join(" ; ")),
                true,
            );
        } else if let Err(e) = save {
            self.notify(format!("Sauvegarde de la session impossible (les fichiers sont bien sur le disque) : {e:#}"), true);
        } else if overruns > 0 {
            self.notify(
                format!("Prise enregistrée, mais {overruns} saturations disque compensées par du silence"),
                true,
            );
        } else if xruns > 0 {
            self.notify(
                format!(
                    "Prise enregistrée · {} · mais le pilote audio a signalé {xruns} décrochage{} : augmentez le buffer ou passez en ASIO",
                    fmt_time(max_len, self.sr()),
                    if xruns > 1 { "s" } else { "" }
                ),
                true,
            );
        } else if !released {
            self.notify(
                "Prise enregistrée (la carte son ne répondait plus à l'arrêt)",
                true,
            );
        } else {
            self.notify(
                format!(
                    "Prise enregistrée et sauvegardée · {}",
                    fmt_time(max_len, self.sr())
                ),
                false,
            );
        }
    }

    /// Arrêt de l'enregistrement demandé au clavier : il faut appuyer deux fois.
    pub fn request_stop_recording(&mut self) {
        if !self.is_recording() {
            return;
        }
        if self.now < self.stop_armed_until {
            self.stop_armed_until = 0.0;
            self.stop();
        } else {
            self.stop_armed_until = self.now + 2.0;
            self.notify(
                "Enregistrement en cours — appuyez encore une fois pour l'arrêter",
                true,
            );
        }
    }

    /// Alertes en direct pendant un enregistrement (bandeau rouge en haut).
    pub fn live_alerts(&mut self) -> Vec<String> {
        let mut alerts = Vec::new();
        if !self.is_recording() {
            self.rec_watch = (0, self.now);
            return alerts;
        }
        let frames = self.engine.shared.rec_frames.load(Ordering::Relaxed);
        if frames != self.rec_watch.0 {
            self.rec_watch = (frames, self.now);
        } else if self.now - self.rec_watch.1 > 1.0 {
            alerts.push(
                "PLUS DE SIGNAL DE LA CARTE SON — vérifiez la connexion / l'horloge Dante"
                    .to_string(),
            );
        }
        let ov = self.engine.shared.overruns.load(Ordering::Relaxed);
        if ov > 0 {
            alerts.push(format!("{ov} pertes de données : le disque ne suit pas"));
        }
        if let Some(r) = &self.recorder {
            for e in r.errors() {
                alerts.push(format!("Erreur disque : {e}"));
            }
        }
        if let Some(free) = self.disk_free {
            let armed = self.live.len().max(1) as u64;
            let bps = match self.project.rec_format {
                RecFormat::Int24 => 3,
                RecFormat::Float32 => 4,
            } * self.sr() as u64
                * armed;
            let secs = free / bps.max(1);
            if secs < 600 {
                alerts.push(format!(
                    "Disque presque plein : ≈ {} d'enregistrement restant",
                    fmt_duration(secs)
                ));
            }
        }
        if let Some(e) = self.engine.shared.last_error.lock().clone() {
            alerts.push(format!("Carte son : {e}"));
        }
        alerts
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

    // ---------------------------------------------------------------- montage

    /// Mémorise l'état avant une modification (pour Annuler).
    pub fn checkpoint(&mut self) {
        self.undo.push(Snapshot {
            tracks: self.project.tracks.clone(),
            markers: self.project.markers.clone(),
        });
        if self.undo.len() > 200 {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    /// À appeler après une modification du montage.
    pub fn edited(&mut self) {
        self.project.dirty = true;
        if matches!(self.transport, Transport::Playing { .. }) {
            // Relance la lecture pour entendre le nouveau montage.
            self.seek(self.playhead);
        }
    }

    /// Restaure le montage (prises, repères) d'un instantané, sans jamais toucher à la
    /// configuration actuelle des pistes (armement, entrées, mute/solo, noms) ni
    /// supprimer de piste : une piste supprimée depuis est simplement rétablie.
    fn restore(&mut self, snap: Snapshot) -> Snapshot {
        let cur = Snapshot {
            tracks: self.project.tracks.clone(),
            markers: std::mem::replace(&mut self.project.markers, snap.markers),
        };
        let current = std::mem::take(&mut self.project.tracks);
        self.project.tracks = project::merge_clips(current, snap.tracks);
        let ids: HashSet<u64> = self
            .project
            .tracks
            .iter()
            .flat_map(|t| t.clips.iter().map(|c| c.id))
            .collect();
        self.sel_clips.retain(|c| ids.contains(c));
        self.edited();
        cur
    }

    pub fn undo(&mut self) {
        if !self.is_recording()
            && self.clip_drag.is_none()
            && let Some(s) = self.undo.pop()
        {
            let cur = self.restore(s);
            self.redo.push(cur);
        }
    }

    pub fn redo(&mut self) {
        if !self.is_recording()
            && self.clip_drag.is_none()
            && let Some(s) = self.redo.pop()
        {
            let cur = self.restore(s);
            self.undo.push(cur);
        }
    }

    /// Les nouvelles prises issues d'une coupe partagent la forme d'onde de l'originale.
    fn share_peaks(&mut self, pairs: &[(u64, u64)]) {
        for (old, new) in pairs {
            if let Some(p) = self.peaks.get(old).cloned() {
                self.peaks.insert(*new, p);
            }
        }
    }

    /// Coupe à la tête de lecture : les prises sélectionnées, sinon celles des pistes
    /// sélectionnées, sinon toutes celles sous la tête.
    pub fn split_at_playhead(&mut self) {
        if self.is_recording() {
            return;
        }
        let pos = self.playhead;
        self.checkpoint();
        let pairs = if !self.sel_clips.is_empty() {
            let sel = self.sel_clips.clone();
            self.project.split_at(pos, None, Some(&sel))
        } else if !self.selected_tracks.is_empty() {
            let tr = self.selected_tracks.clone();
            self.project.split_at(pos, Some(&tr), None)
        } else {
            self.project.split_at(pos, None, None)
        };
        if pairs.is_empty() {
            self.undo.pop();
            self.notify("Aucune prise sous la tête de lecture", true);
            return;
        }
        self.share_peaks(&pairs);
        // La sélection suit la partie droite (pratique pour supprimer la fin).
        if !self.sel_clips.is_empty() {
            self.sel_clips = pairs.iter().map(|p| p.1).collect();
        }
        self.edited();
    }

    /// Fondus d'entrée et de sortie identiques sur les prises sélectionnées (`None` = retirer).
    pub fn set_clip_fades(&mut self, ms: Option<u64>) {
        if self.sel_clips.is_empty() || self.is_recording() {
            return;
        }
        self.checkpoint();
        let len = ms.map_or(0, |ms| self.sr() as u64 * ms / 1000);
        for tr in &mut self.project.tracks {
            for c in tr
                .clips
                .iter_mut()
                .filter(|c| self.sel_clips.contains(&c.id))
            {
                c.fade_in = len.min(c.len / 2);
                c.fade_out = len.min(c.len / 2);
            }
        }
        self.edited();
    }

    /// Supprime les prises sélectionnées, ou le contenu de la plage de temps sélectionnée.
    pub fn delete_selection(&mut self) {
        if self.is_recording() {
            return;
        }
        if !self.sel_clips.is_empty() {
            self.checkpoint();
            let sel = std::mem::take(&mut self.sel_clips);
            for tr in &mut self.project.tracks {
                tr.clips.retain(|c| !sel.contains(&c.id));
            }
            self.edited();
        } else if let Some((a, b)) = self.selection {
            self.checkpoint();
            let tracks = (!self.selected_tracks.is_empty()).then(|| self.selected_tracks.clone());
            let pairs = self.project.delete_range(a, b, tracks.as_ref());
            self.share_peaks(&pairs);
            self.edited();
        }
    }

    // ---------------------------------------------------------------- session

    fn load_peaks(&mut self) {
        self.peaks.clear();
        // Une seule analyse par fichier, partagée par toutes les prises qui en viennent.
        let mut by_file: HashMap<String, Peaks> = HashMap::new();
        for tr in &self.project.tracks {
            for c in &tr.clips {
                let p = by_file
                    .entry(c.file.clone())
                    .or_insert_with(|| {
                        let p: Peaks = Arc::new(RwLock::new(PeakData::default()));
                        let path = self.project.clip_path(c);
                        let p2 = p.clone();
                        std::thread::spawn(move || render::compute_peaks(&path, &p2));
                        p
                    })
                    .clone();
                self.peaks.insert(c.id, p);
            }
        }
    }

    /// La session mérite d'être sauvegardée : elle contient de l'audio, ou elle existe
    /// déjà sur le disque (préparation des pistes et entrées avant un concert).
    fn worth_saving(&self) -> bool {
        self.project.has_audio() || self.project.file_path().exists()
    }

    fn autosave(&mut self) {
        if self.project.dirty && self.worth_saving() {
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
        self.undo.clear();
        self.redo.clear();
        self.sel_clips.clear();
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
        self.load_session(&path);
    }

    /// Charge une session (et récupère les prises d'un enregistrement interrompu).
    pub fn load_session(&mut self, path: &Path) {
        match Project::load(path) {
            Ok(mut p) => {
                let recovered = recovery::recover(&mut p);
                self.project = p;
                self.reset_view_for_session();
                self.load_peaks();
                recovery::remember_session(&self.project);
                match recovered {
                    Ok(n) if n > 0 => {
                        self.zoom_to_fit();
                        self.notify(
                            format!("Enregistrement interrompu récupéré : {n} prise(s) restaurée(s) dans « {} »", self.project.name),
                            false,
                        );
                        return;
                    }
                    Err(e) => {
                        self.notify(format!("Récupération incomplète : {e:#}"), true);
                        return;
                    }
                    _ => {}
                }
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
                let id = p.add_track_ch(Some(tr.name.clone()), tr.input, tr.stereo);
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

    /// Ajoute une piste mono ou stéréo sur l'entrée choisie (`None` = sans entrée).
    /// En stéréo, `input` est le premier canal de la paire.
    pub fn add_track_on(&mut self, input: Option<u16>, stereo: bool) {
        self.project.add_track_ch(None, input, stereo);
    }

    pub fn apply_audio(&mut self, prefs: AudioPrefs) -> Result<(), String> {
        if self.is_recording() {
            return Err("Impossible de changer de carte son pendant un enregistrement : arrêtez d'abord la prise.".into());
        }
        self.stop();
        self.engine.pad(PadCmd::StopAll { fade: 0 });
        // Libère d'abord l'ancien pilote (ASIO n'en autorise qu'un à la fois).
        self.engine = Engine::offline();
        let res = match Engine::open(&prefs, &self.board.pad_out()) {
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
                if let Ok(old) = Engine::open(&self.prefs, &self.board.pad_out()) {
                    self.engine = old;
                }
                Err(format!("{e:#}"))
            }
        };
        self.cache.set_sample_rate(self.engine.status.sample_rate);
        self.out_devices = audio::list_devices(self.prefs.host.as_deref()).1;
        self.sync_engine_to_board();
        res
    }

    pub fn open_export(&mut self) {
        if self.is_recording() {
            self.notify("Export indisponible pendant l'enregistrement", true);
            return;
        }
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
            || self.sb.settings_open
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
        if !ctx.egui_wants_keyboard_input() {
            let shift_cmd = egui::Modifiers::COMMAND | egui::Modifiers::SHIFT;
            let (redo, undo) = ctx.input_mut(|i| {
                (
                    i.consume_key(shift_cmd, Key::Z)
                        || i.consume_key(egui::Modifiers::COMMAND, Key::Y),
                    i.consume_key(egui::Modifiers::COMMAND, Key::Z),
                )
            });
            if redo {
                self.redo();
            } else if undo {
                self.undo();
            }
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
        let p = ui.painter().clone();
        w::hline(&p, full.left(), full.right(), full.bottom() - 1.0, t::LINE);

        // Trois zones qui ne se chevauchent jamais : transport au centre, menu + session +
        // onglets à gauche, carte son à droite. Les textes se raccourcissent selon la place.
        let tw = 4.0 * 30.0 + 12.0 + 168.0;
        let center = egui::Rect::from_center_size(full.center(), vec2(tw, full.height()));
        let left =
            egui::Rect::from_min_max(full.min, egui::pos2(center.left() - 12.0, full.bottom()));
        let right =
            egui::Rect::from_min_max(egui::pos2(center.right() + 12.0, full.top()), full.max);

        // Gauche : menu, session, onglets de vue.
        let tab_labels = ["MULTIPISTE", "SOUNDBOARD"];
        let tabs_w = w::view_tabs_width(ui, &tab_labels);
        let mut lui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(left)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        lui.spacing_mut().item_spacing.x = 6.0;
        lui.add_space(6.0);
        let more = w::icon_button(&mut lui, Icon::More, 28.0, BtnStyle::Ghost, true, "Menu");
        egui::Popup::menu(&more)
            .width(250.0)
            .show(|ui| self.main_menu(ui));
        // Place pour le nom : ce qui reste après le menu, la pastille et les onglets.
        let name_max = (lui.available_width() - tabs_w - 34.0).max(40.0);
        let font = t::semibold(12.0);
        let name = w::fit_text(&lui, &self.project.name, &font, name_max);
        let shortened = name != self.project.name;
        let r = lui.label(egui::RichText::new(name).font(font).color(t::TEXT));
        if shortened {
            r.on_hover_text(&self.project.name);
        }
        if self.project.dirty {
            lui.label(
                egui::RichText::new("●")
                    .font(t::regular(9.0))
                    .color(t::TEXT_3),
            )
            .on_hover_text("Modifications non enregistrées (Ctrl+S)");
        }
        let tabs = egui::Rect::from_min_max(
            egui::pos2(left.right() - tabs_w, full.top()),
            egui::pos2(left.right(), full.bottom() - 1.0),
        );
        let mut tui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(tabs)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        let mut tab = self.tab;
        if w::view_tabs(&mut tui, &mut tab, &tab_labels) {
            self.tab = tab;
        }

        // Centre : transport (visible dans les deux onglets : on peut enregistrer en lançant des pads).
        let mut cui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(center)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        self.transport_controls(&mut cui);

        // Droite : carte son (pastille raccourcie pour tenir dans la place restante).
        let mut rui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(right)
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
        );
        rui.add_space(8.0);
        let st = &self.engine.status;
        let ok = st.input.is_some() || st.output.is_some();
        let dev = st
            .input
            .clone()
            .or(st.output.clone())
            .unwrap_or_else(|| "Aucune carte son".into());
        let full_label = if ok {
            format!(
                "{} · {} · {} kHz · {}/{}",
                st.host,
                dev,
                fmt_khz(st.sample_rate),
                st.in_channels,
                st.out_channels
            )
        } else {
            dev.clone()
        };
        let font = t::regular(11.0);
        let label = if ok {
            // On raccourcit le nom de la carte en priorité, pas la fréquence ni les canaux.
            let tail = format!(
                " · {} kHz · {}/{}",
                fmt_khz(st.sample_rate),
                st.in_channels,
                st.out_channels
            );
            let head = format!("{} · ", st.host);
            let fixed = rui
                .painter()
                .layout_no_wrap(format!("{head}{tail}"), font.clone(), t::TEXT_2)
                .size()
                .x;
            let dev_max = (rui.available_width() - 34.0 - fixed).max(30.0);
            format!("{head}{}{tail}", w::fit_text(&rui, &dev, &font, dev_max))
        } else {
            w::fit_text(&rui, &dev, &font, rui.available_width() - 34.0)
        };
        let gal = rui.painter().layout_no_wrap(label, font, t::TEXT_2);
        let (pr, resp) =
            rui.allocate_exact_size(vec2(gal.size().x + 26.0, 24.0), egui::Sense::click());
        let pp = rui.painter();
        if resp.hovered() {
            pp.rect_filled(pr, t::R, t::SURFACE_3);
        }
        pp.rect_filled(
            egui::Rect::from_center_size(
                egui::pos2(pr.left() + 10.0, pr.center().y),
                vec2(6.0, 6.0),
            ),
            1.0,
            if ok { t::PLAY } else { t::REC },
        );
        pp.galley(
            egui::pos2(pr.left() + 19.0, pr.center().y - gal.size().y / 2.0),
            gal,
            t::TEXT_2,
        );
        if resp
            .on_hover_text(format!("{full_label}\nRéglages de la carte son"))
            .clicked()
        {
            self.open_audio();
        }
    }

    fn transport_controls(&mut self, ui: &mut egui::Ui) {
        let recording = self.is_recording();
        let playing = matches!(self.transport, Transport::Playing { .. });
        let stopped = self.is_stopped();
        ui.spacing_mut().item_spacing.x = 2.0;
        if w::icon_button(
            ui,
            Icon::ToStart,
            30.0,
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
            30.0,
            BtnStyle::Ghost,
            !stopped,
            "Stop (Espace) — pendant une prise : deux clics pour arrêter",
        )
        .clicked()
        {
            if recording {
                self.request_stop_recording();
            } else {
                self.stop();
            }
        }
        let play_style = if playing {
            BtnStyle::Active(t::PLAY)
        } else {
            BtnStyle::Ghost
        };
        if w::icon_button(
            ui,
            Icon::Play,
            30.0,
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
                t::mix(t::REC, t::SURFACE, 0.4)
            })
        } else {
            BtnStyle::Tinted(t::REC)
        };
        if w::icon_button(
            ui,
            Icon::Record,
            30.0,
            rec_style,
            true,
            "Enregistrer (R) — pendant une prise : deux clics pour arrêter",
        )
        .clicked()
        {
            if recording {
                self.request_stop_recording();
            } else {
                self.record();
            }
        }
        ui.add_space(10.0);
        let (rect, _) = ui.allocate_exact_size(vec2(168.0, 30.0), egui::Sense::hover());
        let p = ui.painter();
        p.rect_filled(rect, t::R, t::INSET);
        let col = if recording { t::REC } else { t::TEXT };
        w::counter(
            p,
            egui::pos2(rect.left() + 62.0, rect.center().y),
            &fmt_time(self.playhead, self.sr()),
            t::medium(14.0),
            col,
        );
        let sub = match self.transport {
            Transport::Recording { start } => format!(
                "REC {}",
                fmt_time_short(self.playhead.saturating_sub(start), self.sr())
            ),
            _ => match self.selection {
                Some((a, b)) => format!("SÉL {}", fmt_time_short(b - a, self.sr())),
                None => fmt_time_short(self.project.end(), self.sr()),
            },
        };
        p.text(
            egui::pos2(rect.right() - 8.0, rect.center().y),
            Align2::RIGHT_CENTER,
            sub,
            t::medium(9.5),
            if recording { t::REC } else { t::TEXT_3 },
        );
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
        if ui
            .add(egui::Button::new("Enregistrer").shortcut_text("Ctrl+S"))
            .clicked()
        {
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
                egui::Button::new("Exporter…").shortcut_text("Ctrl+E"),
            )
            .clicked()
        {
            self.open_export();
        }
        ui.separator();
        ui.label(
            egui::RichText::new("FORMAT DES PRISES")
                .font(t::semibold(10.5))
                .color(t::TEXT_3),
        )
        .on_hover_text(
            "Format des fichiers écrits sur le disque pendant l'enregistrement (les originaux de la session, jamais modifiés).\nLe format des fichiers finaux (16 bits, 44.1 kHz…) se choisit à l'export.",
        );
        for f in [RecFormat::Int24, RecFormat::Float32] {
            let (label, tip) = match f {
                RecFormat::Int24 => (
                    "WAV 24 bits (recommandé)",
                    "Standard des enregistrements pro : largement assez de dynamique.",
                ),
                RecFormat::Float32 => (
                    "WAV 32 bits flottant",
                    "Un tiers d'espace disque en plus. Utile surtout si les fichiers seront beaucoup retraités ensuite ; la carte son sature de toute façon à 0 dB.",
                ),
            };
            if ui
                .add_enabled(
                    stopped,
                    egui::RadioButton::new(self.project.rec_format == f, label),
                )
                .on_hover_text(tip)
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
                    let led_port = self.board.led_port.clone();
                    self.midi.connect_leds(led_port.as_deref());
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
            // Buffer réellement utilisé par le pilote (même en « Auto »).
            let blk = self
                .engine
                .shared
                .in_block
                .load(Ordering::Relaxed)
                .max(self.engine.shared.out_block.load(Ordering::Relaxed));
            let buffer = match (blk, self.engine.status.buffer) {
                (0, None) => "auto".to_string(),
                (0, Some(b)) => b.to_string(),
                (n, _) => format!(
                    "{n} ({:.1} ms)",
                    n as f64 * 1000.0 / self.engine.status.sample_rate.max(1) as f64
                ),
            };
            ui.label(dim(format!(
                "Session {} kHz · WAV {} · buffer {buffer}",
                fmt_khz(self.project.sample_rate),
                self.project.rec_format.label(),
            )));
            let sh = &self.engine.shared;
            let xr = sh.xruns.load(Ordering::Relaxed) + sh.underruns.load(Ordering::Relaxed);
            let ov = sh.overruns.load(Ordering::Relaxed);
            if xr > 0 || ov > 0 {
                let r = ui.add(
                    egui::Label::new(
                        egui::RichText::new(format!(
                            "⚠ {xr} décrochage{} · {ov} perte{} disque",
                            if xr > 1 { "s" } else { "" },
                            if ov > 1 { "s" } else { "" }
                        ))
                        .font(t::medium(11.5))
                        .color(t::ORANGE),
                    )
                    .sense(egui::Sense::click()),
                );
                if r.on_hover_text(
                    "Pendant la dernière prise ou lecture.\n\
                     Décrochage : coupure signalée par le pilote audio (un peu de son a pu manquer) — \
                     augmentez le buffer ou passez en ASIO.\n\
                     Perte disque : le disque n'a pas suivi (remplacé par du silence, pistes toujours alignées).\n\
                     Clic : remettre à zéro.",
                )
                .clicked()
                {
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
                    let rec: Vec<_> = self
                        .project
                        .tracks
                        .iter()
                        .filter(|t| t.armed && t.input.is_some())
                        .collect();
                    let armed = rec.len().max(1);
                    // Une piste stéréo occupe deux fois plus de place.
                    let channels = rec.iter().map(|t| t.channels()).sum::<usize>().max(1);
                    let bps = match self.project.rec_format {
                        RecFormat::Int24 => 3,
                        RecFormat::Float32 => 4,
                    } * self.sr() as u64
                        * channels as u64;
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

    pub fn dialog_header(ui: &mut egui::Ui, title: &str) -> bool {
        let mut close = false;
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(title.to_uppercase())
                    .font(t::semibold(11.5))
                    .color(t::TEXT),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                close = w::icon_button(
                    ui,
                    Icon::Close,
                    22.0,
                    BtnStyle::Ghost,
                    true,
                    "Fermer (Échap)",
                )
                .clicked();
            });
        });
        let r = ui.available_rect_before_wrap();
        w::hline(
            ui.painter(),
            r.left() - 16.0,
            r.right() + 16.0,
            r.top() + 2.0,
            t::LINE,
        );
        ui.add_space(12.0);
        close
    }

    fn section(ui: &mut egui::Ui, title: &str) {
        ui.add_space(8.0);
        w::section(ui, title);
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
                // Notes affichées sous la grille (retour à la ligne dans la largeur du dialogue).
                let mut hints: Vec<&str> = Vec::new();
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

                    // ---- Sortie multipiste : toutes les paires de tous les périphériques
                    // (en WASAPI, chaque paire de la carte est un périphérique à part).
                    ui.label(egui::RichText::new("Sortie multipiste").color(t::TEXT_2));
                    let cur = match &dlg.prefs.output {
                        Some(name) => {
                            let ch = dlg.outs.iter().find(|o| &o.name == name).map_or(2, |o| o.channels);
                            out_dest_label(name, ch, dlg.prefs.out_left)
                        }
                        None => format!(
                            "Par défaut{}",
                            dlg.default_out.as_ref().map_or(String::new(), |n| format!(" · {n}"))
                        ),
                    };
                    egui::ComboBox::from_id_salt("out").width(combo_w).height(360.0).selected_text(cur).show_ui(ui, |ui| {
                        if ui.selectable_label(dlg.prefs.output.is_none(), "Par défaut (Windows)").clicked() {
                            dlg.prefs.output = None;
                            dlg.prefs.out_left = 0;
                        }
                        for d in dlg.outs.clone() {
                            for l in (0..d.channels.max(2)).step_by(2) {
                                let sel = dlg.prefs.output.as_deref() == Some(&d.name) && dlg.prefs.out_left == l;
                                if ui.selectable_label(sel, out_dest_label(&d.name, d.channels, l)).clicked() {
                                    dlg.prefs.output = Some(d.name.clone());
                                    dlg.prefs.out_left = l;
                                }
                            }
                        }
                    });
                    ui.end_row();

                    // ---- Fréquence : celles que le pilote accepte vraiment.
                    let rates = dlg.rates();
                    let auto_sr = dlg.default_rate();
                    let wasapi = dlg.prefs.host.as_deref() == Some("WASAPI");
                    let asio = dlg.prefs.host.as_deref() == Some("ASIO");
                    let auto_label = |what: Option<String>| match what {
                        Some(v) => format!("Auto · {v}"),
                        None => "Auto".to_string(),
                    };
                    ui.label(egui::RichText::new("Fréquence").color(t::TEXT_2));
                    let auto_sr_txt = auto_sr.map(|r| format!("{} kHz", fmt_khz(r)));
                    let cur = dlg
                        .prefs
                        .sample_rate
                        .map_or(auto_label(auto_sr_txt.clone()), |r| format!("{} kHz", fmt_khz(r)));
                    egui::ComboBox::from_id_salt("sr").width(170.0).selected_text(cur).show_ui(ui, |ui| {
                        if ui
                            .selectable_label(dlg.prefs.sample_rate.is_none(), auto_label(auto_sr_txt.clone()))
                            .on_hover_text("Fréquence actuelle du pilote")
                            .clicked()
                        {
                            dlg.prefs.sample_rate = None;
                        }
                        for &r in &rates {
                            if ui.selectable_label(dlg.prefs.sample_rate == Some(r), format!("{} kHz", fmt_khz(r))).clicked() {
                                dlg.prefs.sample_rate = Some(r);
                            }
                        }
                    });
                    ui.end_row();
                    if wasapi && rates.len() <= 1 {
                        hints.push(
                            "Fréquence : en WASAPI, Windows impose celle réglée dans ses paramètres son (Propriétés de l'appareil → Format avancé). En ASIO, toutes les fréquences de la carte sont proposées.",
                        );
                    }

                    // ---- Buffer : tailles acceptées par le pilote, avec la latence en ms.
                    let ms_sr = dlg.prefs.sample_rate.or(auto_sr).unwrap_or(48000);
                    let ms = |frames: u32| frames as f64 * 1000.0 / ms_sr as f64;
                    let range = dlg.buffer_range();
                    let fixed = range.filter(|(a, b)| a == b).map(|(a, _)| a);
                    let sizes: Vec<u32> = match range {
                        Some((a, b)) if a < b => audio::BUFFER_SIZES
                            .iter()
                            .copied()
                            .filter(|&n| (a..=b).contains(&n))
                            .collect(),
                        Some(_) => Vec::new(),
                        None => audio::BUFFER_SIZES.to_vec(),
                    };
                    // Valeur réellement utilisée, si ce sont les réglages en cours.
                    let live = (dlg.prefs == self.prefs)
                        .then(|| self.engine.shared.in_block.load(Ordering::Relaxed))
                        .filter(|&n| n > 0);
                    let auto_buf = live.or(fixed);
                    let fmt_buf = |n: u32| format!("{n} éch. · {:.1} ms", ms(n));
                    ui.label(egui::RichText::new("Buffer").color(t::TEXT_2));
                    let cur = dlg
                        .prefs
                        .buffer
                        .map_or(auto_label(auto_buf.map(fmt_buf)), fmt_buf);
                    egui::ComboBox::from_id_salt("buf").width(170.0).selected_text(cur).show_ui(ui, |ui| {
                        if ui
                            .selectable_label(dlg.prefs.buffer.is_none(), auto_label(auto_buf.map(fmt_buf)))
                            .on_hover_text(if asio {
                                "Taille réglée dans le panneau du pilote ASIO"
                            } else {
                                "Taille choisie par le pilote"
                            })
                            .clicked()
                        {
                            dlg.prefs.buffer = None;
                        }
                        for &b in &sizes {
                            if ui.selectable_label(dlg.prefs.buffer == Some(b), fmt_buf(b)).clicked() {
                                dlg.prefs.buffer = Some(b);
                            }
                        }
                    });
                    ui.end_row();
                    if fixed.is_some() || (wasapi && sizes.is_empty()) {
                        hints.push(
                            "Buffer : en WASAPI, Windows le fixe lui-même. Pour une latence plus faible et réglable, passez en ASIO.",
                        );
                    }
                    ui.end_row();
                });
                for h in &hints {
                    ui.add_space(6.0);
                    ui.add(
                        egui::Label::new(egui::RichText::new(*h).font(t::regular(11.5)).color(t::TEXT_3))
                            .wrap(),
                    );
                }
                // Ce qui tourne réellement en ce moment.
                {
                    let st = &self.engine.status;
                    let sh = &self.engine.shared;
                    let blk = |n: u32| {
                        if n == 0 {
                            "—".to_string()
                        } else {
                            format!("{n} éch. ({:.1} ms)", n as f64 * 1000.0 / st.sample_rate.max(1) as f64)
                        }
                    };
                    let mut txt = format!("En cours : {} · {} kHz", st.host, fmt_khz(st.sample_rate));
                    if st.input.is_some() {
                        txt += &format!(" · entrée {}", blk(sh.in_block.load(Ordering::Relaxed)));
                    }
                    if st.output.is_some() {
                        txt += &format!(" · sortie {}", blk(sh.out_block.load(Ordering::Relaxed)));
                    }
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new(txt).font(t::regular(11.5)).color(t::TEXT_3));
                }
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
                        if w::primary_button(ui, None, "Appliquer", true).clicked() {
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
                // Contenu défilant si la fenêtre est trop basse : le bouton « Exporter »
                // reste toujours visible.
                let max_h = (ui.ctx().content_rect().height() - 190.0).max(160.0);
                egui::ScrollArea::vertical()
                    .max_height(max_h)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
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
                                                for (k, (id, name, idx)) in
                                                    with_audio.iter().enumerate()
                                                {
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
                                ui.selectable_value(
                                    &mut d.s.sample_rate,
                                    r,
                                    format!("{} kHz", fmt_khz(r)),
                                );
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
                        if w::primary_button(ui, Some(Icon::Export), "Exporter", n_files > 0)
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
                        if w::primary_button(ui, None, "Créer", !d.name.trim().is_empty()).clicked()
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
                let col = if tt.error { t::REC } else { t::ACCENT };
                let resp = egui::Frame::new()
                    .fill(t::SURFACE_3.gamma_multiply(fade))
                    .stroke(egui::Stroke::new(1.0, t::LINE))
                    .corner_radius(t::R)
                    .inner_margin(egui::Margin {
                        left: 16,
                        right: 14,
                        top: 8,
                        bottom: 8,
                    })
                    .shadow(egui::Shadow {
                        offset: [0, 4],
                        blur: 16,
                        spread: 0,
                        color: Color32::from_black_alpha((140.0 * fade) as u8),
                    })
                    .show(ui, |ui| {
                        ui.set_max_width(620.0);
                        ui.label(
                            egui::RichText::new(&tt.text)
                                .font(t::medium(12.0))
                                .color(t::TEXT.gamma_multiply(fade)),
                        );
                    });
                let r = resp.response.rect;
                ui.painter().rect_filled(
                    egui::Rect::from_min_max(r.min, egui::pos2(r.left() + 3.0, r.bottom())),
                    egui::CornerRadius {
                        nw: t::R,
                        sw: t::R,
                        ne: 0,
                        se: 0,
                    },
                    col.gamma_multiply(fade),
                );
            });
    }
}

impl eframe::App for App {
    /// Appelé à chaque image, et aussi quand la fenêtre est réduite : les tâches de fond
    /// (libération mémoire du moteur, MIDI, LED, sons chargés) ne s'arrêtent jamais.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.now = ctx.input(|i| i.time);
            self.engine.collect_garbage();
            if self.cache.poll() {
                self.sb.midi_dirty = true;
            }
            self.update_playhead();
            self.soundboard_background(ctx);
        }));
        if res.is_err() {
            self.notify("Erreur interne (tâches de fond) — l'audio continue", true);
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // Une erreur dans l'interface ne doit jamais interrompre l'enregistrement :
        // les threads audio et disque sont indépendants, on rattrape l'erreur et on continue.
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.frame(ui)));
        if let Err(e) = res {
            let msg = e
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "erreur inconnue".into());
            self.notify(
                format!("Erreur interne de l'interface (l'audio continue) : {msg}"),
                true,
            );
        }
        // 60 images/s quand la tête avance, 30 sinon (vumètres fluides, moins de charge).
        let ms = if self.is_stopped() { 33 } else { 16 };
        ctx.request_repaint_after(std::time::Duration::from_millis(ms));
    }

    fn on_exit(&mut self) {
        self.midi.leds_off(self.board.led_mode);
        self.stop();
        self.autosave();
        self.board_changed = Some(0.0);
        self.save_board_now();
    }
}

impl App {
    fn frame(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        // (temps, mémoire, MIDI, LED : traités dans `logic`, appelé juste avant)
        self.update_meters(&ctx);
        self.global_keys(&ctx);
        self.update_title(&ctx);
        if let Some(tch) = self.board_changed
            && self.now - tch > 1.0
        {
            self.save_board_now();
        }

        // Fermeture de la fenêtre pendant une prise : on demande confirmation.
        if ctx.input(|i| i.viewport().close_requested()) && self.is_recording() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm_close = true;
        }
        // Sauvegarde automatique des modifications (montage, pistes…).
        if !self.is_recording()
            && self.project.dirty
            && self.worth_saving()
            && self.now - self.last_autosave > 30.0
        {
            self.last_autosave = self.now;
            let _ = self.project.save();
        }

        egui::Panel::top("top")
            .exact_size(44.0)
            .frame(egui::Frame::new().fill(t::SURFACE))
            .show(ui, |ui| self.top_bar(ui));
        let alerts = self.live_alerts();
        if !alerts.is_empty() {
            let blink = (self.now * 2.0).fract() < 0.6;
            egui::Panel::top("alerts")
                .exact_size(26.0)
                .frame(
                    egui::Frame::new()
                        .fill(if blink {
                            t::REC
                        } else {
                            t::mix(t::REC, t::BG, 0.35)
                        })
                        .inner_margin(egui::Margin::symmetric(12, 0)),
                )
                .show(ui, |ui| {
                    ui.horizontal_centered(|ui| {
                        ui.label(
                            egui::RichText::new(alerts.join("   ·   "))
                                .font(t::semibold(11.5))
                                .color(Color32::BLACK),
                        );
                    });
                });
        }
        egui::Panel::bottom("status")
            .exact_size(22.0)
            .frame(
                egui::Frame::new()
                    .fill(t::SURFACE)
                    .inner_margin(egui::Margin::symmetric(12, 0)),
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

        self.confirm_close_dialog(&ctx);
    }

    fn confirm_close_dialog(&mut self, ctx: &egui::Context) {
        if !self.confirm_close {
            return;
        }
        if !self.is_recording() {
            self.confirm_close = false;
            return;
        }
        let mut keep = false;
        let mut quit = false;
        egui::Modal::new(egui::Id::unique("confirm_close")).frame(t::window_frame()).show(ctx, |ui| {
            ui.set_width(420.0);
            ui.label(egui::RichText::new("ENREGISTREMENT EN COURS").font(t::semibold(12.0)).color(t::REC));
            ui.add_space(8.0);
            ui.label("Fermer TracksPlus arrêterait la prise. Les fichiers déjà enregistrés seraient conservés.");
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                if w::primary_button(ui, None, "Continuer l'enregistrement", true).clicked() {
                    keep = true;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Arrêter, sauvegarder et quitter").clicked() {
                        quit = true;
                    }
                });
            });
        });
        if keep {
            self.confirm_close = false;
        }
        if quit {
            self.confirm_close = false;
            self.stop();
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
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

/// Libellé d'une destination de sortie : le nom du périphérique s'il n'a que deux
/// canaux (cas WASAPI), sinon le nom et la paire (cas ASIO, Dante…).
pub fn out_dest_label(device: &str, channels: u16, left: u16) -> String {
    if channels <= 2 {
        device.to_string()
    } else {
        format!("{device} · Sorties {}–{}", left + 1, left + 2)
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
