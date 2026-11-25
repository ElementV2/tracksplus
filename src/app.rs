//! Application : état global, transport, sessions, barre supérieure et fenêtres.
//! Les onglets sont dans `ui::multitrack` et `ui::soundboard`.

use crate::audio::{self, AudioPrefs, DeviceInfo, Engine, RecTarget, Shared};
use crate::export::{
    BitDepth, ExportFormat, ExportJob, ExportSettings, Mp3Bitrate, Normalize, OggQuality,
    SAMPLE_RATES,
};
use crate::midi::Midi;
use crate::player::Player;
use crate::project::{self, Clip, PROJECT_EXT, Project, RecFormat};
use crate::recorder::{FinishError, RecResult, RecSpec, Recorder};
use crate::recovery;
use crate::render::{self, PeakData, Peaks};
use crate::sampler::PadCmd;
use crate::soundboard::{Board, SampleCache};
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
    /// Haut et bas de chaque piste (relatifs au haut de la liste), image précédente :
    /// les pistes dépliées sont plus hautes que les autres.
    pub row_spans: Vec<(f32, f32)>,
}

/// Dialogue « Nouvelles pistes » : plusieurs pistes d'un coup, sur des entrées qui se
/// suivent (Dante 1, 2, 3…), comme dans les stations audio.
pub struct NewTracksDialog {
    pub count: u32,
    pub stereo: bool,
    /// Entrée de la première piste ; les suivantes prennent les entrées d'après.
    pub first: Option<u16>,
    /// Nom de base (vide = « Piste N »).
    pub name: String,
    pub focus: bool,
}

pub struct SettingsDialog {
    pub prefs: AudioPrefs,
    pub hosts: Vec<String>,
    pub ins: Vec<DeviceInfo>,
    pub outs: Vec<DeviceInfo>,
    /// Périphériques utilisés quand l'entrée / la sortie est « Par défaut ».
    pub default_in: Option<String>,
    pub default_out: Option<String>,
    pub listed_host: Option<String>,
    pub error: Option<String>,
    /// Onglet affiché : 0 = carte son, 1 = enregistrement, 2 = soundboard.
    pub tab: usize,
}

impl SettingsDialog {
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
            tab: 0,
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

/// Fin de prise en cours en arrière-plan.
pub struct Finishing {
    rx: std::sync::mpsc::Receiver<Result<Vec<RecResult>, FinishError>>,
    /// Position de la prise sur la timeline (déjà recalée).
    pub start: u64,
    released: bool,
}

/// Résultat d'un import de soundboard : soundboard adopté, sons introuvables.
pub type ImportDone = (Board, Vec<PathBuf>);

/// Résultat d'un « Enregistrer sous » : session copiée, soundboard adopté, sons introuvables,
/// prises introuvables.
pub type SaveAsDone = (Project, Board, Vec<PathBuf>, Vec<String>);

/// Raccourci de choix des pistes à exporter (`Custom` = cochées à la main).
#[derive(Clone, Copy, PartialEq, Eq)]
enum TrackChoice {
    All,
    None,
    /// Pistes sélectionnées dans la colonne de gauche.
    Selected,
    Custom,
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
    /// Pad en cours de déplacement.
    pub drag_pad: Option<u16>,
    /// Zones des onglets de page (cibles de dépôt d'un pad).
    pub page_tabs: Vec<Rect>,
    /// Page dont la suppression attend confirmation.
    pub confirm_delete_page: Option<usize>,
    /// Page pour laquelle la table MIDI a été construite (disposition du contrôleur).
    pub midi_page: Option<usize>,
    /// Contrôleur déjà passé dans sa disposition (mode « Programmer »).
    pub layout_port: Option<String>,
    /// Dernier envoi des LED du contrôleur.
    pub led_at: f64,
    /// Gros sons en cours de copie dans la session (le pad joue l'original en attendant).
    pub copies: Vec<crate::ui::soundboard::PendingCopy>,
    /// Pad renommé directement dans la grille (double-clic) : emplacement, nom en cours.
    pub renaming: Option<(u16, String)>,
    /// Dernier pad supprimé (emplacement, pad) : Ctrl+Z dans le soundboard le rétablit.
    pub deleted: Option<(u16, crate::soundboard::Pad)>,
    /// Début de la panique en fondu (voir `stop_all_pads`).
    pub panic_at: f64,
}

/// Secondes de tampon disque par piste : comme les enregistreurs pro (Ardour 10 s par
/// défaut, jusqu'à 20 s sur disque lent), le plus possible dans un budget de 256 Mo,
/// entre 4 et 15 s. Un disque qui a un trou d'une dizaine de secondes (veille, analyse
/// antivirus, réindexation) ne fait rien perdre.
fn record_buffer_secs(sr: u32, channels: usize) -> usize {
    const BUDGET: usize = 256 << 20;
    let per_sec = sr as usize * channels.max(1) * std::mem::size_of::<f32>();
    (BUDGET / per_sec.max(1)).clamp(4, 15)
}

/// Travail du thread de la copie de sécurité de la session.
pub struct BackupJob {
    project: Project,
    dir: PathBuf,
    journal: Option<recovery::Journal>,
    done: bool,
}

/// Mesure de la place libre : disque de la session, et second disque de la copie de
/// sécurité (`None` = pas de copie, `Some(Err)` = second disque introuvable).
pub struct DiskSpace {
    main: Option<u64>,
    backup: Option<Result<u64, ()>>,
}

/// `a` et `b` sont sur le même disque (volume). Windows : même lecteur ou même partage
/// réseau ; ailleurs : même périphérique de stockage.
pub fn same_volume(a: &Path, b: &Path) -> bool {
    #[cfg(windows)]
    {
        use std::path::Component;
        let vol = |p: &Path| match p.components().next() {
            Some(Component::Prefix(x)) => Some(x.as_os_str().to_string_lossy().to_lowercase()),
            _ => None,
        };
        let (va, vb) = (vol(a), vol(b));
        va.is_some() && va == vb
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let dev = |p: &Path| {
            let mut q = p;
            loop {
                if let Ok(m) = std::fs::metadata(q) {
                    return Some(m.dev());
                }
                q = q.parent()?;
            }
        };
        let (da, db) = (dev(a), dev(b));
        da.is_some() && da == db
    }
}

/// Place libre sur le disque qui contient (ou contiendra) `dir`.
fn free_space(dir: &Path) -> Option<u64> {
    let mut p: &Path = dir;
    while !p.exists() {
        p = p.parent()?;
    }
    fs4::available_space(p).ok()
}

const OUTPUT_STALLED: &str =
    "La sortie audio ne répond pas : vérifiez la carte son (Réglages audio pour la relancer)";

pub struct App {
    pub tab: usize,
    pub project: Project,
    pub engine: Engine,
    pub prefs: AudioPrefs,
    pub transport: Transport,
    pub playhead: u64,
    pub player: Option<Player>,
    /// Position de la timeline où la lecture en cours a commencé (trame 0 du moteur).
    play_base: u64,
    /// Rendus remplacés (mute / solo) : gardés le temps que le moteur bascule.
    retired: Vec<(Player, f64)>,
    pub recorder: Option<Recorder>,
    pub live: Vec<LiveTake>,
    pub peaks: HashMap<u64, Peaks>,
    pub in_meters: Vec<MeterState>,
    /// Niveaux de lecture des pistes audio (par identifiant de piste).
    pub track_meters: HashMap<u64, [MeterState; 2]>,
    /// Position de lecture à la dernière mise à jour des vumètres de pistes.
    meters_pos: u64,
    /// Import de fichiers audio sur la timeline en cours (glisser-déposer).
    pub audio_import: Option<crate::import::ImportJob>,
    /// Fichiers déposés qui ne sont pas de l'audio (signalés à la fin de l'import).
    import_skipped: Vec<PathBuf>,
    /// Piste audio visée par le dépôt (un seul fichier déposé dessus).
    import_target: Option<u64>,
    /// Instant de la dernière mise à jour des vumètres.
    pub meters_at: f64,
    /// Périphériques de sortie du pilote en cours (choix des destinations d'écoute).
    pub out_devices: Vec<DeviceInfo>,
    /// Dernier choix mono / stéréo du menu « Ajouter une piste ».
    pub add_stereo: bool,
    /// Repère dont le menu (renommer / supprimer) est ouvert.
    pub marker_editing: Option<u64>,
    /// Pistes dépliées (panneau de réglages sous l'en-tête).
    pub expanded_tracks: HashSet<u64>,
    /// Dialogue « Nouvelles pistes » ouvert.
    pub tracks_dlg: Option<NewTracksDialog>,
    /// Fin de prise en cours dans un thread (écriture du reste et fermeture des fichiers) :
    /// l'interface ne se fige pas au Stop.
    pub finishing: Option<Finishing>,
    /// « Enregistrer sous » en cours dans un thread (copie des fichiers).
    pub save_as_job: Option<std::sync::mpsc::Receiver<Result<SaveAsDone, String>>>,
    /// Le disque n'a pas répondu à la fin d'une prise : son thread d'écriture tourne peut-être
    /// encore, on n'enregistre plus avant un redémarrage (récupération propre).
    pub disk_stalled: bool,
    /// Dernière alerte « sauvegarde impossible » (pour ne pas la répéter en boucle).
    pub save_error_at: f64,
    /// Dernier changement de session refusé faute de sauvegarde (voir `save_before_leaving`).
    pub leave_anyway_at: f64,
    /// Dossier de la copie de sécurité des prises (second disque), réglage de l'ordinateur.
    pub backup_root: Option<PathBuf>,
    /// Problèmes de la copie de sécurité pendant la dernière prise (affichés à la fin).
    pub backup_warn: Vec<String>,
    /// Thread qui tient à jour le double de la session sur le second disque.
    pub backup_jobs: Option<std::sync::mpsc::Sender<BackupJob>>,
    pub backup_thread: Option<std::thread::JoinHandle<()>>,
    /// Carte son perdue : prochain essai de réouverture (secondes) et délai courant.
    pub reconnect: Option<(f64, f64)>,
    /// Mesure de l'espace disque libre en cours (thread).
    pub disk_job: Option<(f64, PathBuf, std::sync::mpsc::Receiver<DiskSpace>)>,
    /// Latence aller-retour à compenser sur les prises en cours (trames, 0 = aucune).
    pub rec_latency: u64,
    /// La prise en cours est faite en écoutant les pistes existantes.
    pub rec_overdub: bool,
    /// Journal de la prise en cours, à compléter du décalage mesuré dès que la prise a
    /// réellement commencé (prise faite en écoutant).
    pub journal_pending: Option<recovery::Journal>,
    /// Import d'un soundboard en cours (copie des sons dans la session).
    pub import_job: Option<std::sync::mpsc::Receiver<Result<ImportDone, String>>>,
    /// Explorateur intégré (sons d'un pad, session à ouvrir).
    pub browser: Option<crate::ui::browser::FileBrowser>,
    /// Écran d'accueil (au lancement, ou depuis le menu).
    pub welcome: Option<crate::ui::welcome::Welcome>,
    /// Lancement : écran d'accueil en pleine page, aucune session affichée tant que
    /// l'utilisateur n'a pas choisi.
    pub start_screen: bool,
    /// Mises à jour : recherche / téléchargement, réglages, fenêtre ouverte.
    pub updater: crate::update::Updater,
    pub update_settings: crate::update::Settings,
    pub update_dlg: bool,
    /// Recherche automatique du lancement déjà faite.
    pub update_checked: bool,
    /// « Installer et redémarrer » demandé : l'installeur sera lancé dès qu'il est prêt.
    pub update_install: bool,
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
    /// Point d'entrée (`true`) ou de sortie (`false`) posé seul, en attente de l'autre
    /// pour former la plage.
    pub ruler_anchor: Option<(u64, bool)>,
    /// La tête de lecture est tenue à la souris (sur la règle).
    pub playhead_drag: bool,
    /// Pendant une lecture : dernier repositionnement du son pendant le glissé (s).
    pub scrub_at: f64,
    /// Rectangle de sélection des prises en cours (glisser sur les pistes).
    pub marquee: Option<crate::ui::multitrack::Marquee>,
    pub settings_dlg: Option<SettingsDialog>,
    pub export_dlg: Option<ExportDialog>,
    pub export_job: Option<ExportJob>,
    pub new_dlg: Option<NewSessionDialog>,
    toast: Option<Toast>,
    pub ctx_clip: Option<(u64, u64)>,
    pub disk_free: Option<u64>,
    /// Copie de sécurité : place libre sur le second disque (`Err` = disque introuvable).
    pub backup_free: Option<Result<u64, ()>>,
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
        let mut prefs = AudioPrefs::load();
        // Réglages du contrôleur (communs) ; les pads viennent de la session ouverte.
        let board = Board::load_devices();
        // Reprise : la sortie du soundboard était enregistrée avec le soundboard ; elle est
        // désormais dans les réglages audio, avec les autres sorties.
        if prefs.pad_output.is_none()
            && prefs.pad_left == 0
            && (board.out_device.is_some() || board.out_left != 0)
        {
            prefs.pad_output = board.out_device.clone().or_else(|| prefs.output.clone());
            prefs.pad_left = board.out_left;
            prefs.save();
        }
        let (engine, err) = match Engine::open(&prefs, &prefs.pad_out()) {
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
            play_base: 0,
            retired: Vec::new(),
            track_meters: HashMap::new(),
            meters_pos: 0,
            audio_import: None,
            import_skipped: Vec::new(),
            import_target: None,
            recorder: None,
            live: Vec::new(),
            peaks: HashMap::new(),
            in_meters: vec![MeterState::default(); audio::MAX_INPUTS],
            meters_at: 0.0,
            add_stereo: false,
            out_devices: Vec::new(),
            marker_editing: None,
            expanded_tracks: HashSet::new(),
            tracks_dlg: None,
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
                row_spans: Vec::new(),
            },
            selection: None,
            selected_tracks: HashSet::new(),
            show_bridge: false,
            ruler_anchor: None,
            marquee: None,
            playhead_drag: false,
            scrub_at: 0.0,
            settings_dlg: None,
            export_dlg: None,
            export_job: None,
            new_dlg: None,
            toast: None,
            ctx_clip: None,
            disk_free: None,
            backup_free: None,
            disk_checked: -100.0,
            title: String::new(),
            export_last: None,
            now: 0.0,

            stop_armed_until: 0.0,
            confirm_close: false,
            welcome: None,
            finishing: None,
            save_as_job: None,
            disk_stalled: false,
            save_error_at: f64::NEG_INFINITY,
            leave_anyway_at: f64::NEG_INFINITY,
            backup_root: crate::sessions::StartupPrefs::load().backup_dir,
            backup_warn: Vec::new(),
            backup_jobs: None,
            backup_thread: None,
            reconnect: None,
            disk_job: None,
            rec_latency: 0,
            rec_overdub: false,
            journal_pending: None,
            import_job: None,
            browser: None,
            start_screen: false,
            updater: Default::default(),
            update_settings: crate::update::Settings::load(),
            update_dlg: false,
            update_checked: false,
            update_install: false,
            rec_watch: (0, 0.0),
            last_autosave: 0.0,
            awake: None,
            board,
            board_changed: None,
            cache,
            midi: Midi::new(),
            midi_ports: crate::midi::list_ports(),
            sb: SbState {
                zoom: (0.0, 1.0),
                midi_dirty: true,
                panic_at: f64::NEG_INFINITY,
                ..Default::default()
            },
            undo: Vec::new(),
            redo: Vec::new(),
            sel_clips: HashSet::new(),
            snap: false,
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
            // Protégé : une session illisible ne doit pas empêcher l'application de démarrer
            // (sinon elle planterait à chaque lancement, le journal étant toujours là).
            let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                app.load_session(&path);
            }))
            .is_ok();
            if !ok {
                app.notify(
                    format!(
                        "Récupération de « {} » impossible : ouvrez-la à la main",
                        path.display()
                    ),
                    true,
                );
            }
        } else {
            // Écran d'accueil : aucune session tant que l'utilisateur n'a pas choisi de
            // reprendre, créer, ouvrir ou choisir une session.
            app.welcome = Some(crate::ui::welcome::Welcome::with_ctx(&cc.egui_ctx));
            app.start_screen = true;
        }
        if let Some(e) = err {
            let mut d = SettingsDialog::new(&app.prefs);
            d.error = Some(e);
            app.settings_dlg = Some(d);
        }
        app
    }

    /// Message en bas de la fenêtre. Une information disparaît seule ; une erreur ou un
    /// avertissement reste affiché jusqu'au clic : on ne peut pas le manquer.
    pub fn notify(&mut self, text: impl Into<String>, error: bool) {
        self.toast = Some(Toast {
            text: text.into(),
            error,
            until: if error { f64::INFINITY } else { self.now + 3.5 },
        });
    }

    pub fn sr(&self) -> u32 {
        self.engine.status.sample_rate.max(1)
    }

    /// Envoie au moteur la sortie du soundboard et au MIDI le canal.
    pub fn sync_engine_to_board(&mut self) {
        let out_ch = self.engine.status.pad_out_channels.max(1) as usize;
        self.engine.pad(PadCmd::SetOutput {
            left: (self.prefs.pad_out().left as usize).min(out_ch - 1),
        });
        self.midi.shared.lock().tx = self.engine.pad_tx.clone();
        self.sb.midi_dirty = true;
    }

    /// Libère de la mémoire les sons qu'aucun pad n'utilise plus.
    fn retain_sounds(&mut self) {
        let keep: HashSet<PathBuf> = self.board.all_pads().map(|(_, p)| p.path.clone()).collect();
        self.cache.retain(&keep);
    }

    pub fn board_modified(&mut self) {
        self.retain_sounds();
        self.board_changed = Some(self.now);
        self.sb.midi_dirty = true;
    }

    // ---------------------------------------------------------------- transport

    pub fn is_stopped(&self) -> bool {
        self.transport == Transport::Stopped && self.finishing.is_none()
    }

    pub fn is_recording(&self) -> bool {
        matches!(self.transport, Transport::Recording { .. })
    }

    /// Montage interdit : prise en cours, en finalisation (ses prises ne sont pas encore dans
    /// la session : une modification et son annulation les perdraient), ou « Enregistrer
    /// sous » en cours.
    pub fn editing_locked(&self) -> bool {
        self.is_recording()
            || self.finishing.is_some()
            || self.save_as_job.is_some()
            || self.audio_import.is_some()
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
        if !self.engine.start_playback(cons) {
            // Jamais « lecture » à l'écran si le moteur n'a rien reçu.
            self.notify(OUTPUT_STALLED, true);
            return;
        }
        self.player = Some(player);
        self.play_base = self.playhead;
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
                if !self.engine.start_playback(cons) {
                    self.notify(OUTPUT_STALLED, true);
                    self.stop();
                    return;
                }
                self.player = Some(player);
                self.play_base = pos;
                self.transport = Transport::Playing { start: pos };
            }
            Transport::Recording { .. } => {}
        }
    }

    /// Mute / solo changés. Le lecteur lit jusqu'à 3 s en avance (sécurité disque) :
    /// pendant la lecture, un nouveau rendu démarre ~0,2 s devant ce qu'on entend et le
    /// moteur bascule dessus à cette trame précise, avec un fondu court. Le changement
    /// s'entend ainsi aussitôt, sans coupure ni saut.
    pub fn update_audible(&mut self) {
        let audible = self.project.audible();
        let Some(p) = &self.player else {
            return;
        };
        // Repli : l'ancien rendu applique aussi le changement (au-delà de son avance).
        for (a, v) in p.audible.iter().zip(&audible) {
            a.store(*v, Ordering::Relaxed);
        }
        if self.is_stopped() {
            return;
        }
        let margin = self.project.sample_rate as u64 / 5;
        let at = self.engine.shared.out_frames.load(Ordering::Relaxed) + margin;
        let (player, cons) = Player::start_ahead(&self.project, self.play_base + at, audible);
        if self.engine.switch_playback(cons, at)
            && let Some(old) = self.player.replace(player)
        {
            self.retired.push((old, self.now + 5.0));
        }
    }

    pub fn record(&mut self) {
        if self.finishing.is_some() {
            self.notify("Finalisation de la prise précédente en cours…", false);
            return;
        }
        if self.disk_stalled {
            self.notify(
                "Le disque ne répondait plus à la fin de la dernière prise : relancez TracksPlus avant d'enregistrer à nouveau (les prises seront récupérées)",
                true,
            );
            return;
        }
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
        let ring_secs = record_buffer_secs(sr, armed.iter().map(|a| if a.3 { 2 } else { 1 }).sum());
        // Copie de sécurité : un second tampon par piste, rempli par le callback audio
        // lui-même (même taille que le principal) — si le disque principal se bloque, la
        // copie continue.
        let backup = self.backup_session_dir();
        let mut copy_conss = Vec::new();
        for (track_id, name, input, stereo) in armed {
            // Nom unique sur le disque et dans cette prise, sans tenir compte de la casse.
            let file = self.project.new_take_file_avoiding(&name, &used);
            used.insert(file.to_lowercase());
            let width: u16 = if stereo { 2 } else { 1 };
            let (prod, cons) = rtrb::RingBuffer::new(sr as usize * ring_secs * width as usize);
            let peaks: Peaks = Arc::new(RwLock::new(PeakData::default()));
            specs.push(RecSpec {
                track_id,
                path: self.project.audio_dir().join(&file),
                file,
                peaks: peaks.clone(),
                cons,
                channels: width,
            });
            let copy = backup.is_some().then(|| {
                let (p, c) = rtrb::RingBuffer::new(sr as usize * ring_secs * width as usize);
                copy_conss.push(c);
                p
            });
            targets.push(
                if stereo {
                    RecTarget::stereo(input as usize, prod)
                } else {
                    RecTarget::new(input as usize, prod)
                }
                .with_copy(copy),
            );
            self.live.push(LiveTake { track_id, peaks });
        }
        // Sécurité : la session et le journal des prises sont écrits AVANT de lancer
        // l'enregistrement, pour pouvoir tout retrouver après un crash.
        let journal = recovery::Journal {
            start: self.playhead,
            play_offset: 0,
            latency: 0,
            sample_rate: sr,
            takes: specs
                .iter()
                .map(|s| recovery::JournalTake {
                    track_id: s.track_id,
                    file: s.file.clone(),
                })
                .collect(),
        };
        if let Err(e) = self.project.save() {
            self.notify(
                format!("Attention : session non sauvegardée avant la prise ({e:#})"),
                true,
            );
        }
        recovery::remember_session(&self.project);
        let journal_copy = journal.clone();
        if let Err(e) = recovery::write_journal(&self.project.dir, &journal) {
            self.notify(
                format!("Attention : journal de secours non écrit ({e:#})"),
                true,
            );
        }
        // Copie de sécurité : double de la session (fichier de session et journal compris)
        // sur le second disque, écrit dans un thread — un disque en veille ne retarde pas
        // la prise ; s'il est absent, la prise principale se fait quand même.
        self.backup_session_job(Some(journal_copy.clone()), false);
        self.backup_warn.clear();
        if self.backup_root.is_some() && backup.is_none() {
            self.notify(
                "Copie de sécurité inactive pour cette session : elle est sur le même disque que la copie",
                false,
            );
        }
        let recorder = match Recorder::start_with_backup(
            specs,
            sr,
            self.project.rec_format,
            backup.map(|d| (d.join(project::AUDIO_DIR), copy_conss)),
        ) {
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
        // La lecture des pistes existantes démarre AVANT la prise (son pré-remplissage peut
        // prendre du temps) ; le décalage réel entre les deux est mesuré par le callback
        // d'entrée et compensé à la fin, avec la latence entrée + sortie de la carte.
        self.rec_latency = 0;
        self.rec_overdub = false;
        if self.engine.has_output() && self.project.has_audio() {
            let (player, cons) =
                Player::start(&self.project, self.playhead, self.project.audible());
            if self.engine.start_playback(cons) {
                self.player = Some(player);
                self.play_base = self.playhead;
                self.rec_overdub = true;
                self.rec_latency = self.engine.round_trip_latency() as u64;
            } else {
                // Sortie bloquée : la prise se fait quand même, sans la lecture des pistes
                // (donc sans compensation de latence : rien n'a été joué).
                self.notify(OUTPUT_STALLED, true);
            }
        }
        if let Err(e) = self.engine.start_record(targets) {
            recovery::remove_journal(&self.project.dir);
            let _ = recorder.finish();
            self.engine.stop_playback();
            self.player = None;
            self.live.clear();
            self.notify(format!("{e:#}"), true);
            return;
        }
        self.recorder = Some(recorder);
        self.journal_pending = self.rec_overdub.then_some(journal_copy);
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
                // 2) Le thread disque écrit tout ce qui reste puis ferme les fichiers, dans
                //    un thread : l'interface ne se fige pas, même si le disque est lent.
                self.awake = None;
                match self.recorder.take() {
                    Some(rec) => {
                        self.backup_warn = rec
                            .errors()
                            .into_iter()
                            .filter(|e| e.starts_with("copie"))
                            .collect();
                        if self.engine.shared.copy_overruns.load(Ordering::Relaxed) > 0 {
                            self.backup_warn.push(
                                "le second disque n'a pas suivi (silence inséré dans la copie)"
                                    .into(),
                            );
                        }
                        let (tx, rx) = std::sync::mpsc::channel();
                        std::thread::spawn(move || {
                            let _ = tx.send(rec.finish());
                        });
                        // Prise faite en écoutant : elle a commencé à cette position de lecture.
                        let offset = if std::mem::take(&mut self.rec_overdub) {
                            self.engine.shared.rec_play_offset.load(Ordering::Relaxed)
                        } else {
                            0
                        };
                        self.finishing = Some(Finishing {
                            rx,
                            start: start + offset,
                            released,
                        });
                    }
                    None => self.live_takes_to_clips(start, None, released),
                }
            }
        }
        self.transport = Transport::Stopped;
        self.engine.shared.counting.store(false, Ordering::Relaxed);
    }

    /// Prise faite en écoutant : dès qu'elle a commencé, le journal de secours reçoit le
    /// décalage mesuré et la latence, pour qu'une prise récupérée après un crash tombe en
    /// place elle aussi.
    fn complete_journal(&mut self) {
        if !self.is_recording() || self.engine.shared.rec_frames.load(Ordering::Acquire) == 0 {
            return;
        }
        let Some(mut j) = self.journal_pending.take() else {
            return;
        };
        j.play_offset = self.engine.shared.rec_play_offset.load(Ordering::Relaxed);
        j.latency = self.rec_latency;
        let _ = recovery::write_journal(&self.project.dir, &j);
        // Journal complété aussi sur la copie de sécurité (prise recalée correctement si
        // c'est elle qu'on récupère).
        self.backup_session_job(Some(j.clone()), false);
    }

    /// Fin de prise terminée en arrière-plan : on intègre les prises. `wait` : attendre
    /// (fermeture de l'application).
    fn poll_finishing(&mut self, wait: bool) {
        let Some(f) = &self.finishing else {
            return;
        };
        use std::sync::mpsc::{RecvTimeoutError, TryRecvError};
        let res = if wait {
            match f.rx.recv_timeout(std::time::Duration::from_secs(15)) {
                Ok(r) => r,
                Err(RecvTimeoutError::Timeout) => Err(FinishError::Timeout),
                Err(RecvTimeoutError::Disconnected) => Err(FinishError::Panicked),
            }
        } else {
            match f.rx.try_recv() {
                Ok(r) => r,
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => Err(FinishError::Panicked),
            }
        };
        let f = self.finishing.take().expect("prise en finalisation");
        self.live_takes_to_clips(f.start, Some(res), f.released);
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
                // Le thread d'écriture tourne peut-être encore : pas de nouvelle prise d'ici là.
                self.disk_stalled = true;
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
        // Recalage des prises faites en écoutant d'autres pistes (latence aller-retour).
        let lat = std::mem::take(&mut self.rec_latency);
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
            // fichier, chacune à sa position dans la prise (bout à bout, ou avec un trou
            // après une erreur d'écriture).
            let take_len = r.frames();
            for seg in &r.segments {
                let _ = std::fs::remove_file(
                    self.project
                        .audio_dir()
                        .join(crate::recorder::start_sidecar(&seg.file)),
                );
                // Fichier vide, ou entièrement avant la latence (morceau d'avant le début,
                // coupé par une erreur) : rien à poser.
                if seg.frames == 0 || (seg.start + seg.frames <= lat && take_len > lat) {
                    let _ = std::fs::remove_file(self.project.audio_dir().join(&seg.file));
                    continue;
                }
                let id = self.project.alloc_id();
                if let Some(p) = &peaks {
                    // La forme d'onde en direct couvre toute la prise ; chaque fichier en a
                    // une à lui, calculée à part pour rester exacte.
                    let path = self.project.audio_dir().join(&seg.file);
                    // (La forme d'onde en direct couvre la prise depuis sa première trame :
                    // valable seulement si ce fichier est toute la prise.)
                    if r.segments.len() == 1 && seg.start == 0 && seg.frames == take_len {
                        // Fichier fermé : sa forme d'onde est mise en cache pour les
                        // prochaines ouvertures de la session.
                        p.write().shrink();
                        render::store_peaks(&path, &p.read());
                        self.peaks.insert(id, p.clone());
                    } else {
                        let pk: Peaks = Arc::new(RwLock::new(PeakData::default()));
                        render::request_peaks(path, pk.clone());
                        self.peaks.insert(id, pk);
                    }
                }
                // Tout est décalé de la latence : le début de la prise (ce qui tombe avant
                // la latence) est sauté, la prise tombe en place.
                let skip = lat
                    .saturating_sub(seg.start)
                    .min(seg.frames.saturating_sub(project::MIN_CLIP));
                let pos = (start + seg.start + skip).saturating_sub(lat).max(start);
                if let Some(tr) = self.project.track_mut(r.track_id) {
                    tr.clips.push(Clip {
                        id,
                        file: seg.file.clone(),
                        start: pos,
                        offset: skip,
                        len: seg.frames - skip,
                        src_len: seg.frames,
                        fade_in: 0,
                        fade_out: 0,
                    });
                }
            }
            max_len = max_len.max(r.frames());
        }
        self.live.clear();
        self.undo.clear();
        self.redo.clear();
        self.playhead = start + max_len;
        self.project.dirty = true;
        let save = self.project.save();
        if save.is_ok() {
            // Les prises sont référencées dans la session : le journal n'est plus utile.
            recovery::remove_journal(&self.project.dir);
            self.sync_backup_session();
        }
        let overruns = self.engine.shared.overruns.swap(0, Ordering::Relaxed);
        // Décrochages du pilote pendant la prise (ils restent affichés en bas d'écran).
        let xruns = self.engine.shared.xruns.load(Ordering::Relaxed);
        if !errors.is_empty() {
            let mut msg = format!("Erreur d'enregistrement : {}", errors.join(" ; "));
            if let Err(e) = &save {
                msg += &format!(" · Session non sauvegardée : {e:#}");
            }
            self.notify(msg, true);
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
        } else if !self.backup_warn.is_empty() {
            let w = std::mem::take(&mut self.backup_warn);
            self.notify(
                format!(
                    "Prise enregistrée sur le disque principal · Copie de sécurité incomplète : {}",
                    w.join(" ; ")
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
    /// Carte son disparue (DVS relancé, câble USB, pilote réinitialisé) : comme les
    /// logiciels pro, on la rouvre tout seul. Jamais pendant une prise (elle se termine
    /// proprement par Stop, la suite est récupérable) ; la lecture, elle, est arrêtée.
    fn watch_device(&mut self) {
        use crate::audio::{LOST_OUTPUT, LOST_PADS};
        let lost = self.engine.shared.device_lost.load(Ordering::Acquire);
        if lost != 0 && self.reconnect.is_none() {
            self.reconnect = Some((self.now + 1.0, 1.0));
            // Lecture multipiste arrêtée seulement si c'est sa sortie qui a disparu.
            if lost & LOST_OUTPUT != 0 && matches!(self.transport, Transport::Playing { .. }) {
                self.stop();
            }
            if !self.is_recording() {
                self.notify("Carte son perdue : reconnexion automatique…", true);
            }
        }
        let Some((at, delay)) = self.reconnect else {
            return;
        };
        if self.now < at || !self.is_stopped() {
            return;
        }
        // Rouvrir la carte coupe tous les pads : on attend qu'ils aient fini, sauf si
        // c'est justement leur sortie qui a disparu (ils sont muets de toute façon).
        let pads_dead =
            lost & LOST_PADS != 0 || (lost & LOST_OUTPUT != 0 && self.prefs.pad_output.is_none());
        if self.pads_status().0 > 0 && !pads_dead {
            return;
        }
        self.engine.pad(PadCmd::StopAll);
        // L'ancien pilote est libéré avant (ASIO n'en accepte qu'un à la fois).
        self.engine = Engine::offline();
        let opened = Engine::open(&self.prefs, &self.prefs.pad_out());
        let complete = match &opened {
            // Tous les périphériques choisis sont-ils revenus ? (Sinon le moteur tourne avec
            // ce qui est là, et on réessaie : DVS met parfois plusieurs secondes.)
            Ok(e) => {
                let back =
                    |want: &Option<String>, got: &Option<String>| want.is_none() || want == got;
                back(&self.prefs.input, &e.status.input)
                    && back(&self.prefs.output, &e.status.output)
            }
            Err(_) => false,
        };
        if let Ok(e) = opened {
            self.engine = e;
            self.cache.set_sample_rate(self.engine.status.sample_rate);
            self.sync_engine_to_board();
        }
        if complete {
            self.reconnect = None;
            self.notify("Carte son reconnectée", false);
        } else {
            // Essais de plus en plus espacés (jusqu'à 10 s) : l'interface ne saccade pas
            // tant que la carte est absente.
            self.reconnect = Some((self.now + delay, (delay * 2.0).min(10.0)));
        }
    }

    /// Onglet « Enregistrement » des réglages : copie de sécurité sur un second disque.
    /// Désactivée par défaut ; réglage de l'ordinateur (pas de la session).
    fn rec_settings(&mut self, ui: &mut egui::Ui) {
        let stopped = self.is_stopped();
        ui.label(
            egui::RichText::new("COPIE DE SÉCURITÉ")
                .font(t::caps(10.0))
                .color(t::TEXT_3),
        );
        ui.add_space(4.0);
        let mut on = self.backup_root.is_some();
        let resp = ui
            .add_enabled(
                stopped,
                egui::Checkbox::new(
                    &mut on,
                    "Enregistrer aussi chaque prise sur un second disque",
                ),
            )
            .on_disabled_hover_text("Arrêtez d'abord la prise");
        if resp.changed() {
            if on {
                self.pick_backup_root();
            } else {
                self.set_backup_root(None);
            }
        }
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(
                "Comme les enregistreurs pro : chaque prise est écrite en même temps sur un \
                 second disque (clé, SSD externe), avec un double de la session qui s'ouvre tel \
                 quel. Un second disque lent, plein ou débranché ne gêne jamais \
                 l'enregistrement principal.",
            )
            .color(t::TEXT_3),
        );
        if let Some(root) = self.backup_root.clone() {
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Dossier").color(t::TEXT_2));
                ui.label(egui::RichText::new(root.display().to_string()).font(t::medium(12.0)));
            });
            ui.add_space(4.0);
            if ui
                .add_enabled(stopped, egui::Button::new("Changer de dossier…"))
                .clicked()
            {
                self.pick_backup_root();
            }
            ui.add_space(6.0);
            let (txt, col) = match self.backup_free {
                Some(Ok(free)) => (
                    format!(
                        "{:.0} Go libres · ≈ {} d'enregistrement avec les pistes armées",
                        free as f64 / 1e9,
                        fmt_duration(free / self.rec_bytes_per_sec())
                    ),
                    t::TEXT_3,
                ),
                Some(Err(())) => (
                    "Second disque introuvable : branchez-le".to_string(),
                    t::REC,
                ),
                None => ("Mesure de la place libre…".to_string(), t::TEXT_3),
            };
            ui.label(egui::RichText::new(txt).color(col));
        }
    }

    /// Choix du dossier de la copie (sélecteur du système) ; refusé sur le disque de la
    /// session elle-même.
    fn pick_backup_root(&mut self) {
        let Some(d) = rfd::FileDialog::new()
            .set_title("Dossier de la copie de sécurité (second disque)")
            .pick_folder()
        else {
            return;
        };
        let sessions = project::default_sessions_root();
        if same_volume(&d, &self.project.dir) || same_volume(&d, &sessions) {
            // Même disque : la copie ne protégerait de rien si ce disque lâche.
            self.notify(
                "La copie de sécurité doit être sur un AUTRE disque que celui des sessions \
                 (clé USB, SSD externe, second disque interne)",
                true,
            );
        } else {
            self.set_backup_root(Some(d));
        }
    }

    fn set_backup_root(&mut self, d: Option<PathBuf>) {
        let mut prefs = crate::sessions::StartupPrefs::load();
        prefs.backup_dir = d.clone();
        prefs.save();
        self.notify(
            match &d {
                Some(d) => format!("Copie de sécurité activée : {}", d.display()),
                None => "Copie de sécurité désactivée".to_string(),
            },
            false,
        );
        self.backup_root = d;
        // Nouvelle mesure du second disque au prochain passage (dans les 2 s).
        self.backup_free = None;
        self.disk_checked = f64::NEG_INFINITY;
    }

    /// Dossier de la session dans la copie de sécurité : `<second disque>/<session> (code)`,
    /// le code distinguant deux sessions de même nom rangées à des endroits différents.
    /// `None` si la session est elle-même sur le disque de secours (copie ouverte après
    /// une panne du disque principal) : jamais deux écritures dans le même dossier.
    pub fn backup_session_dir(&self) -> Option<PathBuf> {
        let root = self.backup_root.as_ref()?;
        // Comparaison sans accès disque (le second disque peut être en veille) ; casse et
        // séparateurs ignorés comme sous Windows.
        let norm = |p: &Path| {
            p.to_string_lossy()
                .replace('\\', "/")
                .trim_end_matches('/')
                .to_lowercase()
        };
        let (r, d) = (norm(root), norm(&self.project.dir));
        let inside = |a: &str, b: &str| a == b || a.starts_with(&format!("{b}/"));
        if inside(&d, &r) || inside(&r, &d) {
            return None;
        }
        // Session ouverte sur le disque de la copie (Windows : comparaison des lecteurs,
        // sans accès disque) : pas de copie sur le même disque.
        #[cfg(windows)]
        if same_volume(root, &self.project.dir) {
            return None;
        }
        // FNV-1a : code stable d'une version à l'autre de l'application.
        let code = d.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
            (h ^ b as u64).wrapping_mul(0x0100_0000_01b3)
        });
        Some(root.join(format!(
            "{} ({:08x})",
            project::sanitize(&self.project.name),
            code as u32
        )))
    }

    /// Met à jour le double de la session sur le second disque : fichier de session, et
    /// journal de la prise en cours (`journal`) ou son retrait (`done`). Un seul thread,
    /// travaux dans l'ordre : un ancien état n'écrase jamais un plus récent, même si le
    /// second disque est lent à se réveiller.
    fn backup_session_job(&mut self, journal: Option<recovery::Journal>, done: bool) {
        let Some(dir) = self.backup_session_dir() else {
            return;
        };
        let tx = self.backup_jobs.get_or_insert_with(|| {
            let (tx, rx) = std::sync::mpsc::channel::<BackupJob>();
            let h = std::thread::Builder::new()
                .name("tracksplus-backup-session".into())
                .spawn(move || {
                    for mut job in rx {
                        job.project.dir = job.dir.clone();
                        let saved = job.project.save().is_ok();
                        if let Some(j) = &job.journal {
                            let _ = recovery::write_journal(&job.dir, j);
                        } else if job.done && saved {
                            recovery::remove_journal(&job.dir);
                        }
                    }
                })
                .ok();
            self.backup_thread = h;
            tx
        });
        let _ = tx.send(BackupJob {
            project: self.project.clone(),
            dir,
            journal,
            done,
        });
    }

    fn sync_backup_session(&mut self) {
        self.backup_session_job(None, true);
    }

    /// Prise illisible pendant la lecture (fichier absent, déplacé ou abîmé) : elle est
    /// jouée en silence, on le dit au lieu de laisser croire à une piste vide.
    fn poll_play_errors(&mut self) {
        let errs: Vec<String> = match &self.player {
            Some(p) => p.errors.try_iter().take(8).collect(),
            None => return,
        };
        if let Some(first) = errs.first() {
            let more = if errs.len() > 1 {
                format!(" (+{} autre(s))", errs.len() - 1)
            } else {
                String::new()
            };
            self.notify(
                format!("Lecture : prise illisible, jouée en silence — {first}{more}"),
                true,
            );
        }
    }

    /// Débit d'écriture d'une prise avec les pistes armées (octets / s, au moins 1) :
    /// une piste stéréo compte double.
    fn rec_bytes_per_sec(&self) -> u64 {
        let channels: usize = self
            .project
            .tracks
            .iter()
            .filter(|t| t.armed && t.input.is_some())
            .map(|t| t.channels())
            .sum();
        let bytes = match self.project.rec_format {
            RecFormat::Int24 => 3,
            RecFormat::Float32 => 4,
        };
        (bytes * self.sr() as u64 * channels.max(1) as u64).max(1)
    }

    pub fn live_alerts(&mut self) -> Vec<String> {
        let mut alerts = Vec::new();
        if !self.is_recording() {
            self.rec_watch = (0, self.now);
            return alerts;
        }
        if self.recorder.as_ref().is_some_and(|r| !r.is_alive()) {
            alerts.push(
                "Écriture disque arrêtée (erreur interne) : arrêtez la prise, elle sera récupérée"
                    .into(),
            );
        }
        let frames = self.engine.shared.rec_frames.load(Ordering::Relaxed);
        if frames != self.rec_watch.0 {
            self.rec_watch = (frames, self.now);
        } else if self.now - self.rec_watch.1 > 1.0 {
            let dante = self
                .engine
                .status
                .input
                .as_deref()
                .is_some_and(|n| n.to_lowercase().contains("dante"));
            alerts.push(if dante {
                "PLUS DE SIGNAL DE LA CARTE SON — vérifiez la connexion et l'horloge Dante"
                    .to_string()
            } else {
                "PLUS DE SIGNAL DE LA CARTE SON — vérifiez la connexion".to_string()
            });
        }
        let ov = self.engine.shared.overruns.load(Ordering::Relaxed);
        if ov > 0 {
            alerts.push(format!("{ov} pertes de données : le disque ne suit pas"));
        }
        if self.engine.shared.copy_overruns.load(Ordering::Relaxed) > 0 {
            alerts.push(
                "Copie de sécurité : le second disque ne suit pas (la prise principale continue)"
                    .into(),
            );
        }
        if let Some(r) = &self.recorder {
            for e in r.errors() {
                alerts.push(format!("Erreur disque : {e}"));
            }
        }
        let bps = self.rec_bytes_per_sec();
        if let Some(free) = self.disk_free {
            let secs = free / bps;
            // Alerte dès 30 min restantes (comme les enregistreurs pro) : le temps de
            // libérer de la place ou de prévoir un autre disque avant la fin du concert.
            if secs < 1800 {
                alerts.push(format!(
                    "Disque presque plein : ≈ {} d'enregistrement restant",
                    fmt_duration(secs)
                ));
            }
        }
        match self.backup_free {
            Some(Ok(free)) if free / bps < 1800 => alerts.push(format!(
                "Copie de sécurité : second disque presque plein, ≈ {} restant (la prise principale continue)",
                fmt_duration(free / bps)
            )),
            Some(Err(())) => alerts.push(
                "Copie de sécurité : second disque introuvable (la prise principale continue)"
                    .into(),
            ),
            _ => {}
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
        let center = |i: usize| {
            self.view
                .row_spans
                .get(i)
                .map_or(i as f32 * h + h * 0.5, |(a, b)| (a + b) / 2.0)
        };
        let visible = rows.iter().any(|&i| {
            let y = center(i);
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
        if !self.editing_locked()
            && self.clip_drag.is_none()
            && let Some(s) = self.undo.pop()
        {
            let cur = self.restore(s);
            self.redo.push(cur);
        }
    }

    pub fn redo(&mut self) {
        if !self.editing_locked()
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
        if self.editing_locked() {
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
        if self.sel_clips.is_empty() || self.editing_locked() {
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
        if self.editing_locked() {
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
                        // File d'attente (2 threads) et cache sur le disque : rouvrir une
                        // session ne relit plus tout l'audio.
                        render::request_peaks(self.project.clip_path(c), p.clone());
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

    pub fn autosave(&mut self) {
        if self.project.dirty
            && self.worth_saving()
            && let Err(e) = self.project.save()
        {
            self.save_failed(e);
        }
        self.save_board_now();
    }

    /// Sauvegarde automatique impossible (disque plein, clé retirée…) : on le dit, sans
    /// répéter le message en boucle. La session reste marquée « modifiée ».
    fn save_failed(&mut self, e: anyhow::Error) {
        self.project.dirty = true;
        if self.now - self.save_error_at > 30.0 {
            self.save_error_at = self.now;
            self.notify(format!("Sauvegarde de la session impossible : {e:#}"), true);
        }
    }

    /// Enregistre le soundboard : réglages du contrôleur pour l'ordinateur, pads dans la
    /// session (qui est créée sur le disque si c'est son premier contenu).
    pub fn save_board_now(&mut self) -> bool {
        if self.board_changed.take().is_none() {
            return true;
        }
        if let Err(e) = self.board.save_devices() {
            self.notify(
                format!("Réglages du soundboard non sauvegardés : {e:#}"),
                true,
            );
        }
        if !self.board.has_pads() && !self.project.file_path().exists() {
            return true;
        }
        if !self.project.file_path().exists() {
            if let Err(e) = self.project.save() {
                self.notify(format!("{e:#}"), true);
                self.board_changed = Some(self.now + 29.0);
                return false;
            }
            recovery::remember_session(&self.project);
        }
        if let Err(e) = self.board.save_session(&self.project.dir) {
            self.notify(format!("Soundboard non sauvegardé : {e:#}"), true);
            // Toujours à sauvegarder : nouvel essai à la prochaine sauvegarde automatique.
            self.board_changed = Some(self.now + 29.0);
            return false;
        }
        true
    }

    /// Avant d'ouvrir ou de créer une autre session : tout est sauvegardé, sinon on
    /// n'abandonne pas la session en cours (disque plein, clé retirée…). Un second essai
    /// dans les 10 s passe outre, pour ne jamais rester bloqué.
    pub fn save_before_leaving(&mut self) -> bool {
        self.save_before(
            "Session en cours non sauvegardée : changement annulé. Recommencez pour \
             changer quand même (les modifications non sauvegardées seront perdues).",
        )
    }

    /// Comme `save_before_leaving`, avec le message à afficher en cas d'échec.
    pub fn save_before(&mut self, refused: &str) -> bool {
        let mut ok = true;
        if self.project.dirty
            && self.worth_saving()
            && let Err(e) = self.project.save()
        {
            self.project.dirty = true;
            ok = false;
            self.notify(format!("Sauvegarde de la session impossible : {e:#}"), true);
        }
        ok &= self.save_board_now();
        if ok {
            return true;
        }
        if self.now - self.leave_anyway_at < 10.0 {
            self.leave_anyway_at = f64::NEG_INFINITY;
            return true;
        }
        self.leave_anyway_at = self.now;
        self.notify(refused, true);
        false
    }

    /// Soundboard de la session qui vient d'être ouverte ou créée (les réglages du
    /// contrôleur restent ceux de l'ordinateur).
    fn load_session_board(&mut self) {
        self.engine.pad(PadCmd::StopAll);
        let fresh = !self
            .project
            .dir
            .join(crate::soundboard::BOARD_FILE)
            .exists();
        let mut b = match Board::load_session(&self.project.dir) {
            Ok(b) => b,
            Err(e) => {
                self.notify(format!("Soundboard de la session illisible : {e:#}"), true);
                Board::default()
            }
        };
        b.take_devices(&self.board);
        if fresh {
            // Nouvelle session : même taille de grille que jusqu'ici.
            b.cols = self.board.cols;
            b.rows = self.board.rows;
        }
        self.set_board(b);
    }

    /// Remplace le soundboard affiché (sons à charger, MIDI, sortie).
    fn set_board(&mut self, b: Board) {
        self.engine.pad(PadCmd::StopAll);
        self.sb.deleted = None;
        self.board = b;
        self.retain_sounds();
        self.board_changed = None;
        self.sb.selected = None;
        self.sb.page = 0;
        self.sb.renaming = None;
        self.sb.drag_pad = None;
        self.sb.learn_key = None;
        self.sb.learn_midi = None;
        self.sb.held_keys.clear();
        self.sb.midi_dirty = true;
        let paths: Vec<PathBuf> = self.board.all_pads().map(|(_, p)| p.path.clone()).collect();
        for p in paths {
            self.cache.request(&p, None);
        }
        self.sync_engine_to_board();
    }

    /// Importe un soundboard (ancien soundboard commun, ou celui d'une autre session) dans
    /// la session en cours : ses sons y sont copiés.
    fn import_board(&mut self) {
        let start = Board::legacy_backup_path()
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .unwrap_or_else(project::default_sessions_root);
        let Some(path) = rfd::FileDialog::new()
            .set_title("Importer un soundboard")
            .add_filter("Soundboard", &["json"])
            .set_directory(start)
            .pick_file()
        else {
            return;
        };
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let mut b = match Board::load(&path) {
            Ok(b) => b,
            Err(e) => {
                self.notify(format!("{e:#}"), true);
                return;
            }
        };
        for page in &mut b.pages {
            for pad in page.pads.iter_mut().flatten() {
                if pad.path.is_relative() {
                    pad.path = dir.join(&pad.path);
                }
            }
        }
        // Copie des sons dans la session en arrière-plan (fenêtre d'attente).
        let dir = self.project.dir.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let missing = b.adopt_sounds(&dir);
            let _ = tx.send(Ok((b, missing)));
        });
        self.import_job = Some(rx);
    }

    /// Import de soundboard terminé : il remplace le soundboard de la session.
    fn poll_import(&mut self) {
        let Some(rx) = &self.import_job else {
            return;
        };
        let res = match rx.try_recv() {
            Ok(r) => r,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Err("Import du soundboard interrompu (erreur interne)".to_string())
            }
        };
        self.import_job = None;
        let (mut b, missing) = match res {
            Ok(r) => r,
            Err(e) => {
                self.notify(e, true);
                return;
            }
        };
        b.take_devices(&self.board);
        self.set_board(b);
        self.board_changed = Some(self.now);
        self.save_board_now();
        if missing.is_empty() {
            self.notify("Soundboard importé dans la session", false);
        } else {
            self.notify(
                format!(
                    "Soundboard importé · {} son(s) introuvable(s), pads à refaire",
                    missing.len()
                ),
                true,
            );
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
        // Les numéros de pistes recommencent à 1 dans chaque session : rien de l'ancienne
        // ne doit viser une piste de la nouvelle.
        self.expanded_tracks.clear();
        self.renaming_track = None;
        self.ruler_anchor = None;
        self.clip_drag = None;
    }

    /// « Ouvrir une session… » : explorateur intégré (la session choisie s'ouvre).
    pub fn open_session(&mut self) {
        self.close_dialogs();
        self.open_session_browser();
    }

    /// Charge une session (et récupère les prises d'un enregistrement interrompu).
    pub fn load_session(&mut self, path: &Path) {
        match Project::load(path) {
            Ok(mut p) => {
                let recovered = recovery::recover(&mut p);
                self.start_screen = false;
                self.project = p;
                self.load_session_board();
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
            Ok(()) => {
                crate::sessions::remember(&self.project.file_path());
                self.notify(
                    format!("Enregistré · {}", self.project.dir.display()),
                    false,
                )
            }
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
        // Copie des prises et des sons dans un thread (fenêtre de progression) : l'interface
        // ne se fige pas, même avec des Go d'audio.
        let mut p = self.project.clone();
        let mut b = self.board.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let res = p
                .save_as(dir, name)
                .map(|lost| {
                    let missing = b.adopt_sounds(&p.dir);
                    (p, b, missing, lost)
                })
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(res);
        });
        self.save_as_job = Some(rx);
    }

    /// « Enregistrer sous » terminé : la copie devient la session ouverte.
    fn poll_save_as(&mut self) {
        let Some(rx) = &self.save_as_job else {
            return;
        };
        let res = match rx.try_recv() {
            Ok(r) => r,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Err("Enregistrement de la session interrompu (erreur interne)".to_string())
            }
        };
        self.save_as_job = None;
        match res {
            Ok((p, b, missing, lost)) => {
                if !lost.is_empty() {
                    self.notify(
                        format!(
                            "{} prise(s) introuvable(s) non copiée(s) : {}",
                            lost.len(),
                            lost.join(", ")
                        ),
                        true,
                    );
                }
                self.project = p;
                // Des fichiers ont pu être renommés dans la copie : l'historique d'avant
                // pointerait sur de mauvais fichiers.
                self.undo.clear();
                self.redo.clear();
                if !missing.is_empty() {
                    self.notify(
                        format!("{} son(s) du soundboard introuvable(s)", missing.len()),
                        true,
                    );
                }
                // Même soundboard, chemins vers la copie : les pads en cours continuent de
                // jouer (pas de StopAll), page et sélection sont gardées.
                for (slot, pad) in b.all_pads() {
                    if let Some(old) = self.board.pad(slot) {
                        self.cache.alias(&old.path, &pad.path);
                    }
                }
                self.board = b;
                // Le pad supprimé pointerait vers l'ancienne session.
                self.sb.deleted = None;
                self.retain_sounds();
                let paths: Vec<PathBuf> =
                    self.board.all_pads().map(|(_, p)| p.path.clone()).collect();
                for p in paths {
                    self.cache.request(&p, None);
                }
                self.sb.midi_dirty = true;
                self.board_changed = Some(self.now);
                self.save_board_now();
                recovery::remember_session(&self.project);
                self.notify(
                    format!("Session enregistrée · {}", self.project.dir.display()),
                    false,
                );
            }
            Err(e) => self.notify(e, true),
        }
    }

    fn save_as_dialog(&mut self, ctx: &egui::Context) {
        let (title, detail) = if self.save_as_job.is_some() {
            (
                "Enregistrement de la session…",
                "Copie des prises et des sons dans le nouveau dossier.",
            )
        } else if self.import_job.is_some() {
            (
                "Import du soundboard…",
                "Copie des sons dans le dossier de la session.",
            )
        } else {
            return;
        };
        egui::Modal::new(egui::Id::unique("save_as_job"))
            .frame(t::window_frame())
            .show(ctx, |ui| {
                ui.set_width(360.0);
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(16.0).color(t::ACCENT));
                    ui.label(egui::RichText::new(title).font(t::semibold(14.0)));
                });
                ui.add_space(6.0);
                ui.label(egui::RichText::new(detail).color(t::TEXT_3));
            });
        ctx.request_repaint();
    }

    fn create_session(&mut self, d: NewSessionDialog) {
        self.stop();
        if !self.save_before_leaving() {
            return;
        }
        let name = project::sanitize(&d.name);
        let dir = d.parent.join(&name);
        if dir.join(format!("{name}.{PROJECT_EXT}")).exists() {
            self.notify(format!("Une session « {name} » existe déjà ici"), true);
            return;
        }
        let mut p = Project::new(name, dir, self.engine.status.sample_rate);
        p.rec_format = self.project.rec_format;
        if d.keep_tracks {
            // Pistes d'enregistrement seulement : les pistes audio n'ont de sens qu'avec
            // leurs fichiers.
            for tr in self.project.tracks.iter().filter(|t| !t.is_audio()) {
                let id = p.add_track_ch(Some(tr.name.clone()), tr.input, tr.stereo);
                if let Some(nt) = p.track_mut(id) {
                    nt.armed = tr.armed;
                    nt.monitor = tr.monitor;
                }
            }
        }
        if let Err(e) = p.save() {
            self.notify(format!("{e:#}"), true);
            return;
        }
        self.start_screen = false;
        self.project = p;
        self.load_session_board();
        self.peaks.clear();
        self.reset_view_for_session();
        recovery::remember_session(&self.project);
        self.notify(format!("Nouvelle session « {} »", self.project.name), false);
    }

    /// Ouvre « Nouvelles pistes », prérempli : dernier type utilisé, première entrée
    /// libre (paire libre en stéréo).
    pub fn open_new_tracks(&mut self) {
        let stereo = self.add_stereo;
        self.tracks_dlg = Some(NewTracksDialog {
            count: 1,
            stereo,
            first: self.first_free_input(stereo),
            name: String::new(),
            focus: true,
        });
    }

    /// Première entrée (ou paire) qu'aucune piste n'utilise.
    pub fn first_free_input(&self, stereo: bool) -> Option<u16> {
        let used: HashSet<u16> = self
            .project
            .tracks
            .iter()
            .flat_map(|t| t.inputs())
            .collect();
        let step = if stereo { 2 } else { 1 };
        (0..self.engine.status.in_channels)
            .step_by(step)
            .find(|&i| (i..i + step as u16).all(|c| !used.contains(&c)))
            .or((self.engine.status.in_channels > 0).then_some(0))
    }

    /// Crée `count` pistes ; la k-ième prend l'entrée `first + k` (paires en stéréo).
    pub fn create_tracks(&mut self, count: u32, stereo: bool, first: Option<u16>, name: &str) {
        let width: u16 = if stereo { 2 } else { 1 };
        let name = name.trim();
        for k in 0..count {
            let input = first.map(|f| f.saturating_add(k as u16 * width));
            let nm = match (name.is_empty(), count) {
                (true, _) => None,
                (false, 1) => Some(name.to_string()),
                (false, _) => Some(format!("{name} {}", k + 1)),
            };
            self.project.add_track_ch(nm, input, stereo);
        }
        self.add_stereo = stereo;
    }

    pub fn apply_audio(&mut self, prefs: AudioPrefs) -> Result<(), String> {
        if self.editing_locked() {
            return Err("Impossible de changer de carte son pendant un enregistrement : arrêtez d'abord la prise.".into());
        }
        self.stop();
        self.engine.pad(PadCmd::StopAll);
        // Libère d'abord l'ancien pilote (ASIO n'en autorise qu'un à la fois).
        self.engine = Engine::offline();
        let res = match Engine::open(&prefs, &prefs.pad_out()) {
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
                if let Ok(old) = Engine::open(&self.prefs, &self.prefs.pad_out()) {
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

    /// « Actualiser » : rouvre la carte son pour relire les réglages du pilote. Beaucoup de
    /// pilotes (RME notamment) ne donnent un buffer ou une fréquence changés dans leur
    /// panneau qu'à la réouverture. En ASIO, le panneau du pilote fait foi : buffer et
    /// fréquence repassent en « Auto ».
    fn reload_audio(&mut self) {
        if self.is_recording() {
            self.notify(
                "Impossible pendant une prise : arrêtez d'abord l'enregistrement",
                true,
            );
            return;
        }
        let before = self.prefs.clone();
        let mut prefs = before.clone();
        if prefs.host.as_deref() == Some("ASIO") {
            prefs.buffer = None;
            prefs.sample_rate = None;
        }
        match self.apply_audio(prefs) {
            Ok(()) => {
                let msg = format!(
                    "Carte son relancée avec les réglages du pilote ({} kHz)",
                    fmt_khz(self.engine.status.sample_rate)
                );
                self.notify(msg, false);
            }
            Err(e) => self.notify(format!("Carte son : {e}"), true),
        }
        // Fenêtre « Carte son » ouverte : elle montre les nouvelles valeurs.
        let now = self.prefs.clone();
        if let Some(d) = &mut self.settings_dlg {
            if d.prefs == before {
                d.prefs = now;
            }
            d.refresh();
        }
    }

    pub fn open_export(&mut self) {
        if self.editing_locked() {
            self.notify("Export indisponible pendant l'enregistrement", true);
            return;
        }
        if !self.project.has_audio() {
            self.notify("Rien à exporter pour l'instant", true);
            return;
        }
        self.close_dialogs();
        let mut s = self.export_last.clone().unwrap_or_else(|| ExportSettings {
            sample_rate: self.project.sample_rate,
            depth: BitDepth::I24,
            stems: true,
            mix: false,
            dither: true,
            dir: self.project.dir.join("Export"),
            tracks: Vec::new(),
            range: None,
            ..Default::default()
        });
        // Dossier mémorisé relatif à la session : celui d'une autre session (et ses
        // exports) n'est jamais réutilisé ni écrasé.
        if s.dir.is_relative() {
            s.dir = self.project.dir.join(&s.dir);
        }
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
        let audible = self.project.audible();
        let heard: HashSet<u64> = self
            .project
            .tracks
            .iter()
            .zip(&audible)
            .filter(|(t, a)| **a && !t.clips.is_empty())
            .map(|(t, _)| t.id)
            .collect();
        let default_tracks = if !sel.is_empty() {
            sel
        } else if !heard.is_empty() {
            heard
        } else {
            with_audio
        };
        self.export_dlg = Some(ExportDialog {
            s,
            range: if self.selection.is_some() {
                RangeChoice::Selection
            } else {
                RangeChoice::All
            },
            tracks: default_tracks,
        });
    }

    pub fn open_settings(&mut self) {
        self.close_dialogs();
        self.settings_dlg = Some(SettingsDialog::new(&self.prefs));
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

    pub fn close_dialogs(&mut self) {
        self.settings_dlg = None;
        self.export_dlg = None;
        self.new_dlg = None;
        self.update_dlg = false;
        self.browser = None;
    }

    pub fn dialog_open(&self) -> bool {
        self.settings_dlg.is_some()
            || self.tracks_dlg.is_some()
            || self.export_dlg.is_some()
            || self.new_dlg.is_some()
            || self.export_job.is_some()
            || self.update_dlg
            || self.welcome.is_some()
            || self.browser.is_some()
            || self.save_as_job.is_some()
            || self.import_job.is_some()
            || self.audio_import.is_some()
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
            } else if undo && self.tab == 1 && self.sb.deleted.is_some() {
                self.restore_deleted_pad();
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

    /// Indique au moteur audio quels canaux d'entrée sont écoutés en direct, et où
    /// (centre pour une piste mono, gauche / droite pour une piste stéréo).
    fn sync_monitoring(&self) {
        let sh = &self.engine.shared;
        let mut codes = [0u8; audio::MAX_INPUTS];
        let mut put = |c: usize, v: u8| {
            if c < audio::MAX_INPUTS {
                // Canal écouté par deux pistes de nature différente : au centre.
                codes[c] = if codes[c] == 0 || codes[c] == v { v } else { 1 };
            }
        };
        for t in self.project.tracks.iter().filter(|t| t.monitor) {
            if let Some(i) = t.input {
                let i = i as usize;
                if t.stereo {
                    put(i, 2);
                    put(i + 1, 3);
                } else {
                    put(i, 1);
                }
            }
        }
        let mut any = false;
        for (c, &v) in codes.iter().enumerate() {
            any |= v != 0;
            if sh.monitor[c].load(Ordering::Relaxed) != v {
                sh.monitor[c].store(v, Ordering::Relaxed);
            }
        }
        sh.monitor_on.store(any, Ordering::Relaxed);
    }

    /// Quelque chose bouge à l'écran : transport, prise, vumètres, pads, messages…
    fn needs_animation(&self) -> bool {
        // ≈ -60 dB : le bruit de fond d'une entrée ne maintient pas 60 images/s.
        const QUIET: f32 = 1e-3;
        let meters = self
            .in_meters
            .iter()
            .chain(self.master_play.iter())
            .chain(self.master_pad.iter())
            .chain(self.track_meters.values().flatten())
            .any(|m| m.level > QUIET || m.hold > QUIET);
        let pads = self
            .engine
            .shared
            .pad_pos
            .iter()
            .any(|p| p.load(Ordering::Relaxed) != 0);
        !self.is_stopped()
            || meters
            || pads
            || self.toast.as_ref().is_some_and(|t| t.until.is_finite())
            || self.updater.busy()
            || self.export_job.is_some()
            || self.save_as_job.is_some()
            || self.import_job.is_some()
            || self.audio_import.is_some()
            || self.sb.drag_pad.is_some()
    }

    fn update_meters(&mut self) {
        self.sync_monitoring();
        let now_t = self.now;
        self.retired.retain(|(_, until)| *until > now_t);
        // Temps réellement écoulé depuis l'image précédente (et non la durée « prévue »
        // d'egui, fausse quand l'affichage ne tourne pas à 60 images/s) : la vitesse de
        // retour des vumètres est exacte quelle que soit la cadence.
        let now = self.now;
        let dt = ((now - self.meters_at) as f32).clamp(0.0, 0.25);
        self.meters_at = now;
        let sh = &self.engine.shared;
        for ch in 0..(self.engine.status.in_channels as usize).min(audio::MAX_INPUTS) {
            let p = sh.take_meter(ch);
            self.in_meters[ch].update(p, now, dt);
        }
        for k in 0..2 {
            self.master_play[k].update(Shared::take_peak(&sh.master_play[k]), now, dt);
            self.master_pad[k].update(Shared::take_peak(&sh.master_pad[k]), now, dt);
        }
        // Pistes audio : niveau du passage entendu depuis l'image précédente.
        let pos = self.playhead;
        let from = if pos >= self.meters_pos {
            self.meters_pos
        } else {
            pos
        };
        self.meters_pos = pos;
        let playing = !self.is_stopped();
        self.track_meters
            .retain(|id, _| self.project.tracks.iter().any(|t| t.id == *id));
        for (i, t) in self.project.tracks.iter().enumerate() {
            if !t.is_audio() {
                continue;
            }
            let v = self
                .player
                .iter()
                .chain(self.retired.iter().rev().map(|(p, _)| p))
                .filter(|_| playing)
                .find_map(|p| p.meters.peak(i, from, pos))
                .unwrap_or([0.0; 2]);
            let m = self.track_meters.entry(t.id).or_default();
            m[0].update(v[0], now, dt);
            m[1].update(v[1], now, dt);
        }
        // Espace libre mesuré dans un thread : un disque lent, en veille ou réseau
        // déconnecté ne fige jamais l'interface (une seule mesure en cours à la fois).
        if let Some((since, dir, rx)) = &self.disk_job {
            match rx.try_recv() {
                Ok(free) => {
                    if *dir == self.project.dir {
                        self.disk_free = free.main;
                        self.backup_free = free.backup;
                    }
                    self.disk_job = None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.disk_job = None,
                // Mesure bloquée (disque qui ne répond plus) ou session changée : on
                // l'abandonne pour que les suivantes repartent.
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    if now - since > 10.0 || *dir != self.project.dir {
                        self.disk_job = None;
                    }
                }
            }
        }
        if now - self.disk_checked > 2.0 && self.disk_job.is_none() {
            self.disk_checked = now;
            let dir = self.project.dir.clone();
            let backup = self.backup_session_dir();
            let (tx, rx) = std::sync::mpsc::channel();
            let spawned = std::thread::Builder::new()
                .name("tracksplus-disk-space".into())
                .spawn(move || {
                    let main = free_space(&dir);
                    let backup = backup.map(|b| free_space(&b).ok_or(()));
                    let _ = tx.send(DiskSpace { main, backup });
                });
            if spawned.is_ok() {
                self.disk_job = Some((now, self.project.dir.clone(), rx));
            }
        }
    }

    fn update_title(&mut self, ctx: &egui::Context) {
        let tt = if self.start_screen {
            "TracksPlus".to_string()
        } else {
            format!(
                "{}{} — TracksPlus",
                self.project.name,
                if self.project.dirty { " •" } else { "" }
            )
        };
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
        self.update_menu_dot(&lui, more.rect);
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

        // Centre : le transport de l'onglet affiché (multipiste ou pads).
        let mut cui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(center)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        if self.tab == 0 {
            self.transport_controls(&mut cui);
        } else {
            self.pads_transport(&mut cui);
        }

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
            self.open_settings();
        }
        rui.add_space(8.0);
        self.update_pill(&mut rui);
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
            .add_enabled(stopped, egui::Button::new("Ouvrir une session…"))
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
        // Sélecteur de fichier bloquant puis arrêt des pads : jamais pendant le spectacle.
        let idle = self.is_stopped() && self.pads_status().0 == 0 && !self.board.locked;
        if ui
            .add_enabled(idle, egui::Button::new("Importer un soundboard…"))
            .on_disabled_hover_text("Arrêtez la lecture, la prise et les pads d'abord")
            .on_hover_text("Reprend les pads d'un autre soundboard (ancien soundboard, autre session) ; ses sons sont copiés dans cette session")
            .clicked()
        {
            ui.close();
            self.import_board();
        }
        ui.separator();
        if ui.button("Réglages…").clicked() {
            self.open_settings();
        }
        self.update_menu(ui);
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
                let bps = self.rec_bytes_per_sec();
                // Second disque de la copie de sécurité (à droite du disque principal).
                match self.backup_free {
                    Some(Ok(free)) => {
                        let secs = free / bps;
                        ui.label(
                            egui::RichText::new(format!("Copie ≈ {}", fmt_duration(secs)))
                                .font(t::regular(11.5))
                                .color(if secs < 1800 { t::REC } else { t::TEXT_3 }),
                        )
                        .on_hover_text(format!(
                            "Copie de sécurité : {:.0} Go libres sur le second disque",
                            free as f64 / 1e9
                        ));
                    }
                    Some(Err(())) => {
                        ui.label(
                            egui::RichText::new("Copie : second disque introuvable")
                                .font(t::medium(11.5))
                                .color(t::REC),
                        )
                        .on_hover_text(
                            "Branchez le disque de la copie de sécurité, ou désactivez-la (Réglages → Enregistrement)",
                        );
                    }
                    None => {}
                }
                if let Some(free) = self.disk_free {
                    let armed = self
                        .project
                        .tracks
                        .iter()
                        .filter(|t| t.armed && t.input.is_some())
                        .count()
                        .max(1);
                    let secs = free / bps;
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

    fn settings_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dlg) = self.settings_dlg.take() else {
            return;
        };
        let mut close = false;
        let mut apply = false;
        let mut recheck = false;
        egui::Modal::new(egui::Id::unique("settings_dlg"))
            .frame(t::window_frame())
            .show(ctx, |ui| {
                ui.set_width(520.0);
                // Même hauteur pour tous les onglets : la fenêtre ne saute pas d'un onglet à
                // l'autre (boutons en bas).
                const SETTINGS_H: f32 = 530.0;
                let top = ui.cursor().top();
                ui.set_min_height(SETTINGS_H);
                close = Self::dialog_header(ui, "Réglages");
                // Onglets : d'autres réglages de l'application pourront s'y ajouter.
                ui.allocate_ui_with_layout(
                    vec2(ui.available_width(), 30.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        w::view_tabs(ui, &mut dlg.tab, &["CARTE SON", "ENREGISTREMENT", "SOUNDBOARD"]);
                    },
                );
                {
                    let y = ui.cursor().top();
                    let r = ui.max_rect();
                    w::hline(ui.painter(), r.left(), r.right(), y, t::LINE);
                }
                ui.add_space(12.0);
                if dlg.tab == 1 {
                    self.rec_settings(ui);
                    return;
                }
                if dlg.tab == 2 {
                    self.sb_settings(ui);
                    return;
                }
                // Notes affichées sous la grille (retour à la ligne dans la largeur du dialogue).
                let mut hints: Vec<&str> = Vec::new();
                egui::Grid::new("audio_grid").num_columns(2).spacing([16.0, 10.0]).show(ui, |ui| {
                    let combo_w = 320.0;
                    ui.label(egui::RichText::new("Pilote").color(t::TEXT_2));
                    let cur = dlg.prefs.host.clone().unwrap_or_default();
                    egui::ComboBox::from_id_salt("host").width(combo_w).selected_text(&cur).show_ui(ui, |ui| {
                        for h in dlg.hosts.clone() {
                            // Autre pilote = autres noms de périphériques : seul un changement
                            // de pilote remet l'entrée et les sorties par défaut.
                            if cur != h && ui.selectable_label(false, &h).clicked() {
                                dlg.prefs.host = Some(h);
                                dlg.prefs.input = None;
                                dlg.prefs.output = None;
                                dlg.prefs.out_left = 0;
                                dlg.prefs.pad_output = None;
                                dlg.prefs.pad_left = 0;
                                dlg.prefs.monitor_output = None;
                                dlg.prefs.monitor_left = 0;
                            } else if cur == h {
                                let _ = ui.selectable_label(true, &h);
                            }
                        }
                    });
                    ui.end_row();
                    if dlg.listed_host != dlg.prefs.host {
                        dlg.refresh();
                    }

                    ui.label(egui::RichText::new("Entrée").color(t::TEXT_2));
                    let cur = dlg.prefs.input.clone().unwrap_or_else(|| "Par défaut".into());
                    let shown = combo_text(ui, &cur, combo_w);
                    egui::ComboBox::from_id_salt("in").width(combo_w).selected_text(shown).show_ui(ui, |ui| {
                        if ui.selectable_label(dlg.prefs.input.is_none(), "Par défaut").clicked() {
                            dlg.prefs.input = None;
                        }
                        for d in &dlg.ins {
                            let txt = format!("{}   {} canaux", d.name, d.channels);
                            // L'entrée ne touche jamais aux sorties : chacune se règle à part.
                            if ui.selectable_label(dlg.prefs.input.as_deref() == Some(&d.name), txt).clicked() {
                                dlg.prefs.input = Some(d.name.clone());
                            }
                        }
                    });
                    ui.end_row();

                    // ---- Sorties : toutes réglées ici (multipiste, soundboard, monitoring).
                    // Chaque liste montre toutes les paires de tous les périphériques (en
                    // WASAPI, chaque paire d'une carte est un périphérique à part).
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new("SORTIES").font(t::caps(10.0)).color(t::TEXT_3));
                    ui.label("");
                    ui.end_row();
                    let outs = dlg.outs.clone();
                    let default_out = format!(
                        "Par défaut{}",
                        dlg.default_out.as_ref().map_or(String::new(), |n| format!(" · {n}"))
                    );
                    ui.label(egui::RichText::new("Main multipiste").color(t::TEXT_2))
                        .on_hover_text("Lecture des pistes");
                    dest_combo(ui, "out_main", combo_w, &outs, &mut dlg.prefs.output, &mut dlg.prefs.out_left, "Par défaut (Windows)", &default_out);
                    ui.end_row();
                    ui.label(egui::RichText::new("Monitoring multipiste").color(t::TEXT_2))
                        .on_hover_text("Écoute en direct des entrées (bouton casque des pistes)");
                    dest_combo(ui, "out_mon", combo_w, &outs, &mut dlg.prefs.monitor_output, &mut dlg.prefs.monitor_left, "Comme le main multipiste", "Comme le main multipiste");
                    ui.end_row();
                    ui.label(egui::RichText::new("Soundboard").color(t::TEXT_2))
                        .on_hover_text("Lecture des pads");
                    dest_combo(ui, "out_pads", combo_w, &outs, &mut dlg.prefs.pad_output, &mut dlg.prefs.pad_left, "Comme le main multipiste", "Comme le main multipiste");
                    ui.end_row();
                    ui.add_space(4.0);
                    ui.label("");
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
                    // ASIO n'ouvre qu'un pilote à la fois : on prévient (sans rien changer)
                    // si l'entrée et une sortie sont sur des pilotes différents.
                    if asio {
                        let input = dlg.prefs.input.clone().or(dlg.default_in.clone());
                        let main = dlg.prefs.output.clone().or(dlg.default_out.clone());
                        let others = [dlg.prefs.pad_output.clone(), dlg.prefs.monitor_output.clone()];
                        let differs = |o: &Option<String>| o.is_some() && input.is_some() && *o != input;
                        if differs(&main) || others.iter().any(differs) {
                            hints.push(
                                "ASIO : un seul pilote à la fois. Choisissez l'entrée et les sorties sur la même carte, sinon l'ouverture échouera.",
                            );
                        }
                    }
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
                    if wasapi && (fixed.is_some() || sizes.is_empty()) {
                        hints.push(
                            "Buffer : en WASAPI, Windows le fixe lui-même. Pour une latence plus faible et réglable, passez en ASIO.",
                        );
                    } else if asio && fixed.is_some() {
                        hints.push(
                            "Buffer : ce pilote n'accepte que la taille réglée dans son propre panneau. Changez-la là-bas, puis cliquez sur « Actualiser ».",
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
                // Rappel Dante seulement si une carte Dante est choisie.
                let dante = [
                    dlg.prefs.input.clone().or(dlg.default_in.clone()),
                    dlg.prefs.output.clone().or(dlg.default_out.clone()),
                    dlg.prefs.pad_output.clone(),
                    dlg.prefs.monitor_output.clone(),
                ]
                .iter()
                .flatten()
                .any(|n| n.to_lowercase().contains("dante"));
                if dante {
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(
                            "Dante : la fréquence doit être la même que dans Dante Controller.",
                        )
                        .font(t::regular(11.5))
                        .color(t::TEXT_3),
                    );
                }
                if let Some(e) = &dlg.error {
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new(e).font(t::medium(12.5)).color(t::REC));
                }
                ui.add_space((top + SETTINGS_H - 26.0 - ui.cursor().top()).max(14.0));
                ui.horizontal(|ui| {
                    if w::secondary_button(ui, "Actualiser").clicked() {
                        dlg.refresh();
                        recheck = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if w::primary_button(ui, None, "Appliquer", true).clicked() {
                            apply = true;
                        }
                    });
                });
            });
        if recheck {
            self.reload_audio();
        }
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
            self.settings_dlg = Some(dlg);
        }
    }

    /// Fichiers audio déposés sur le multipiste : conversion en arrière-plan, puis une
    /// piste audio par fichier (ou ajout à la piste audio `target`), prises posées à `at`.
    pub fn import_audio_files(&mut self, paths: Vec<PathBuf>, at: u64, target: Option<u64>) {
        if self.dialog_open() {
            self.notify(
                "Fermez la fenêtre ouverte avant de déposer des fichiers sur la timeline.",
                true,
            );
            return;
        }
        if self.editing_locked() {
            self.notify(
                "Import impossible pendant un enregistrement ou une copie : réessayez après.",
                true,
            );
            return;
        }
        let (audio, other): (Vec<PathBuf>, Vec<PathBuf>) = paths
            .into_iter()
            .partition(|p| crate::soundboard::is_audio_file(p));
        if audio.is_empty() {
            self.notify(
                format!(
                    "Aucun fichier audio reconnu ({}). Formats acceptés : WAV, AIFF, MP3, FLAC, Ogg, M4A.",
                    other
                        .iter()
                        .filter_map(|p| p.file_name())
                        .map(|n| n.to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                true,
            );
            return;
        }
        self.stop();
        if let Err(e) = self.project.ensure_dirs() {
            self.notify(format!("Import impossible : {e:#}"), true);
            return;
        }
        let mut taken = HashSet::new();
        let targets: Vec<(PathBuf, String)> = audio
            .into_iter()
            .map(|p| {
                let stem = p
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "Import".into());
                let file = self.project.new_take_file_avoiding(&stem, &taken);
                taken.insert(file.clone());
                (p, file)
            })
            .collect();
        self.import_skipped = other;
        self.import_target = target;
        match crate::import::ImportJob::start(
            targets,
            self.project.audio_dir(),
            self.project.sample_rate,
            at,
        ) {
            Ok(job) => self.audio_import = Some(job),
            Err(e) => self.notify(format!("{e:#}"), true),
        }
    }

    /// Fenêtre de progression de l'import, puis création des pistes audio.
    fn audio_import_dialog(&mut self, ctx: &egui::Context) {
        let Some(job) = &self.audio_import else {
            return;
        };
        let p = job.progress.load(Ordering::Relaxed) as f32 / 1000.0;
        let mut cancel = false;
        egui::Modal::new(egui::Id::unique("audio_import"))
            .frame(t::window_frame())
            .show(ctx, |ui| {
                ui.set_width(360.0);
                ui.label(egui::RichText::new("Import en cours").font(t::semibold(16.0)));
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new("Conversion dans le dossier « Audio » de la session")
                        .font(t::regular(12.0))
                        .color(t::TEXT_3),
                );
                ui.add_space(12.0);
                ui.add(
                    egui::ProgressBar::new(p)
                        .desired_width(360.0)
                        .desired_height(8.0)
                        .fill(t::ACCENT),
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(format!("{:.0} %", p * 100.0)).color(t::TEXT_2));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if w::secondary_button(ui, "Annuler").clicked() {
                            cancel = true;
                        }
                    });
                });
            });
        if cancel {
            job.cancel.store(true, Ordering::Relaxed);
        }
        if !job.is_finished() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
            return;
        }
        let job = self.audio_import.take().unwrap();
        let at = job.at;
        let mut done = Vec::new();
        let mut errors: Vec<String> = self
            .import_skipped
            .drain(..)
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .map(|n| format!("{n} : pas un fichier audio"))
            .collect();
        let mut cancelled = false;
        for (src, r) in job.join() {
            let name = src
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            match r {
                Ok(i) => done.push(i),
                Err(e) if e.is::<crate::import::Cancelled>() => cancelled = true,
                Err(e) => errors.push(format!("{name} : {e:#}")),
            }
        }
        let n = done.len();
        if n > 0 {
            self.checkpoint();
            let target = self.import_target.take().filter(|id| {
                self.project
                    .tracks
                    .iter()
                    .any(|t| t.id == *id && t.is_audio())
            });
            for i in done {
                // Un seul fichier déposé sur une piste audio : il va sur cette piste.
                let tid = match target {
                    Some(id) if n == 1 => {
                        if let Some(t) = self.project.track_mut(id) {
                            t.stereo |= i.stereo;
                        }
                        id
                    }
                    _ => self.project.add_audio_track(i.name.clone(), i.stereo),
                };
                let cid = self.project.alloc_id();
                if let Some(t) = self.project.track_mut(tid) {
                    t.clips.push(Clip {
                        id: cid,
                        file: i.file,
                        start: at,
                        offset: 0,
                        len: i.frames,
                        src_len: i.frames,
                        fade_in: 0,
                        fade_out: 0,
                    });
                }
            }
            self.project.dirty = true;
            self.load_peaks();
            if let Err(e) = self.project.save() {
                self.save_failed(e);
            }
        }
        self.import_target = None;
        if !errors.is_empty() {
            self.notify(
                format!(
                    "{}Impossible d'importer {} fichier(s) : {}",
                    if n > 0 {
                        format!("{n} piste(s) audio ajoutée(s). ")
                    } else {
                        String::new()
                    },
                    errors.len(),
                    errors.join(" · ")
                ),
                true,
            );
        } else if cancelled {
            self.notify("Import annulé : rien n'a été ajouté", false);
        } else if n > 0 {
            self.notify(format!("{n} fichier(s) importé(s) sur la timeline"), false);
        }
    }

    fn new_tracks_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut d) = self.tracks_dlg.take() else {
            return;
        };
        let in_ch = self.engine.status.in_channels;
        let mut close = false;
        let mut create = false;
        let resp = egui::Modal::new(egui::Id::unique("tracks_dlg"))
            .frame(t::window_frame())
            .show(ctx, |ui| {
                ui.set_width(440.0);
                close = Self::dialog_header(ui, "Nouvelles pistes");
                egui::Grid::new("tracks_grid")
                    .num_columns(2)
                    .spacing([16.0, 10.0])
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new("Nombre").color(t::TEXT_2));
                        ui.add_sized(
                            [64.0, 22.0],
                            egui::DragValue::new(&mut d.count).range(1..=128).speed(0.1),
                        );
                        ui.end_row();

                        ui.label(egui::RichText::new("Type").color(t::TEXT_2));
                        let was = d.stereo;
                        w::segmented(ui, "nt_type", &mut d.stereo, &[(false, "Mono"), (true, "Stéréo")]);
                        if d.stereo != was {
                            // Paire en stéréo : la première entrée est recalée sur 1-2, 3-4…
                            d.first = d.first.map(|i| if d.stereo { i & !1 } else { i });
                        }
                        ui.end_row();

                        ui.label(egui::RichText::new("Première entrée").color(t::TEXT_2));
                        crate::ui::multitrack::input_combo(
                            ui,
                            "nt_first",
                            260.0,
                            &mut d.first,
                            d.stereo,
                            in_ch,
                            &self.project.tracks,
                            None,
                        );
                        ui.end_row();

                        ui.label(egui::RichText::new("Nom").color(t::TEXT_2));
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut d.name)
                                .desired_width(260.0)
                                .hint_text("Piste (numérotées automatiquement)"),
                        );
                        if d.focus {
                            d.focus = false;
                            r.request_focus();
                        }
                        if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            create = true;
                        }
                        ui.end_row();
                    });

                // Résumé et avertissements : on sait exactement ce qui va être créé.
                let width: u32 = if d.stereo { 2 } else { 1 };
                let kind = if d.stereo { "stéréo" } else { "mono" };
                let plural = if d.count > 1 { "s" } else { "" };
                let mut lines = Vec::new();
                match d.first {
                    Some(f) => {
                        let first = f as u32 + 1;
                        let last = f as u32 + d.count * width;
                        lines.push((
                            format!(
                                "{} piste{plural} {kind}{plural} · entrée{} {first} à {last}",
                                d.count,
                                if last > first { "s" } else { "" }
                            ),
                            t::TEXT_2,
                        ));
                        if last > in_ch as u32 {
                            lines.push((
                                format!(
                                    "La carte actuelle n'a que {in_ch} entrée{} : les pistes au-delà n'enregistreront pas tant qu'elle n'en a pas assez.",
                                    if in_ch > 1 { "s" } else { "" }
                                ),
                                t::ORANGE,
                            ));
                        }
                        let used: HashSet<u16> =
                            self.project.tracks.iter().flat_map(|t| t.inputs()).collect();
                        let taken: Vec<String> = (f as u32..f as u32 + d.count * width)
                            .filter(|c| used.contains(&(*c as u16)))
                            .map(|c| (c + 1).to_string())
                            .collect();
                        if !taken.is_empty() {
                            lines.push((
                                format!("Déjà utilisées par d'autres pistes : {}", taken.join(", ")),
                                t::ORANGE,
                            ));
                        }
                    }
                    None => lines.push((
                        format!("{} piste{plural} {kind}{plural} sans entrée", d.count),
                        t::TEXT_2,
                    )),
                }
                ui.add_space(10.0);
                for (txt, col) in lines {
                    ui.add(
                        egui::Label::new(egui::RichText::new(txt).font(t::regular(11.5)).color(col))
                            .wrap(),
                    );
                }
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    if w::secondary_button(ui, "Annuler").clicked() {
                        close = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let label = if d.count > 1 {
                            format!("Créer {} pistes", d.count)
                        } else {
                            "Créer la piste".into()
                        };
                        if w::primary_button(ui, Some(Icon::Plus), &label, true).clicked() {
                            create = true;
                        }
                    });
                });
            });
        if create {
            self.create_tracks(d.count, d.stereo, d.first, &d.name);
            return;
        }
        if !(close || resp.should_close()) {
            self.tracks_dlg = Some(d);
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
                            if w::secondary_button(ui, "Annuler").clicked() {
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
                    Ok(out) => {
                        let dir = out
                            .files
                            .first()
                            .and_then(|f| f.parent())
                            .map(Path::to_path_buf);
                        if out.warnings.is_empty() {
                            self.notify(
                                format!("Export terminé · {} fichier(s)", out.files.len()),
                                false,
                            );
                        } else {
                            // Export réussi mais à vérifier (saturation…) : message qui reste.
                            self.notify(
                                format!(
                                    "Export terminé · {} fichier(s), à vérifier : {}",
                                    out.files.len(),
                                    out.warnings.join(" · ")
                                ),
                                true,
                            );
                        }
                        if let Some(d) = dir {
                            open_in_file_manager(&d);
                        }
                    }
                    Err(e) if e.is::<crate::export::Cancelled>() => {
                        self.notify("Export annulé : aucun fichier n'a été modifié", false)
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
        if self.selection.is_none() {
            d.range = RangeChoice::All;
        }
        d.s.fit_format();
        // Pistes avec de l'audio : (id, nom, position, canaux).
        let with_audio: Vec<(u64, String, usize, usize)> = self
            .project
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, t)| !t.clips.is_empty())
            .map(|(i, t)| (t.id, t.name.clone(), i, t.channels()))
            .collect();
        let all: HashSet<u64> = with_audio.iter().map(|x| x.0).collect();
        let picked: HashSet<u64> = all
            .iter()
            .copied()
            .filter(|id| self.selected_tracks.contains(id))
            .collect();
        let audible = self.project.audible();
        let muted_tracks = self
            .project
            .tracks
            .iter()
            .zip(&audible)
            .any(|(t, a)| !a && !t.clips.is_empty());
        let session_len = self.project.end();
        let all_label = format!(
            "Toute la session · {}",
            fmt_duration(session_len / sr as u64)
        );
        let sel_label = self.selection.map(|(a, b)| {
            format!(
                "Plage In/Out · {} → {}",
                fmt_time_short(a, sr),
                fmt_time_short(b, sr)
            )
        });
        egui::Modal::new(egui::Id::unique("export_dlg"))
            .frame(t::window_frame())
            .show(ctx, |ui| {
                ui.set_width(660.0);
                close = Self::dialog_header(ui, "Exporter");
                // Contenu défilant si la fenêtre est trop basse : le bouton « Exporter »
                // reste toujours visible.
                let max_h = (ui.ctx().content_rect().height() - 190.0).max(160.0);
                egui::ScrollArea::vertical()
                    .max_height(max_h)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        // ------------------------------------------------ Contenu
                        Self::section(ui, "Contenu");
                        exp_row(ui, "Pistes", |ui| {
                            // Le choix qui correspond aux pistes cochées est en surbrillance
                            // (aucun si elles sont cochées à la main).
                            let mut choice = if d.tracks == all {
                                TrackChoice::All
                            } else if d.tracks.is_empty() {
                                TrackChoice::None
                            } else if !picked.is_empty() && d.tracks == picked {
                                TrackChoice::Selected
                            } else {
                                TrackChoice::Custom
                            };
                            let mut options = vec![
                                (
                                    TrackChoice::All,
                                    "Toutes",
                                    "Toutes les pistes qui contiennent de l'audio.",
                                ),
                                (
                                    TrackChoice::None,
                                    "Aucune",
                                    "Tout décocher, pour choisir les pistes une par une ci-dessous.",
                                ),
                            ];
                            if !picked.is_empty() {
                                options.push((
                                    TrackChoice::Selected,
                                    "Pistes sélectionnées",
                                    "Les pistes sélectionnées dans la fenêtre principale (clic sur leur nom).",
                                ));
                            }
                            if w::segmented_tips(ui, "exp_choice", &mut choice, &options) {
                                d.tracks = match choice {
                                    TrackChoice::All => all.clone(),
                                    TrackChoice::Selected => picked.clone(),
                                    TrackChoice::None | TrackChoice::Custom => HashSet::new(),
                                };
                            }
                            ui.add_space(6.0);
                            ui.label(
                                egui::RichText::new(format!(
                                    "{} / {} cochée(s)",
                                    d.tracks.len(),
                                    with_audio.len()
                                ))
                                .font(t::regular(12.0))
                                .color(t::TEXT_3),
                            );
                        });
                        ui.horizontal_top(|ui| {
                            ui.add_space(EXP_LABEL_W + ui.spacing().item_spacing.x);
                            egui::Frame::new()
                                .fill(t::INSET)
                                .corner_radius(t::R)
                                .inner_margin(8)
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    egui::ScrollArea::vertical()
                                        .max_height(130.0)
                                        .auto_shrink([false, true])
                                        .show(ui, |ui| {
                                            egui::Grid::new("exp_tracks")
                                                .num_columns(3)
                                                .spacing([18.0, 4.0])
                                                .show(ui, |ui| {
                                                    for (k, (id, name, idx, ch)) in
                                                        with_audio.iter().enumerate()
                                                    {
                                                        let mut on = d.tracks.contains(id);
                                                        let txt = egui::RichText::new(format!(
                                                            "{:02}  {}",
                                                            idx + 1,
                                                            short(name, 16)
                                                        ));
                                                        let tip = if *ch == 2 {
                                                            "Piste stéréo : fichier stéréo"
                                                        } else {
                                                            "Piste mono : fichier mono"
                                                        };
                                                        if ui
                                                            .checkbox(&mut on, txt)
                                                            .on_hover_text(tip)
                                                            .changed()
                                                        {
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
                        });
                        ui.add_space(4.0);
                        if muted_tracks {
                            exp_hint(
                                ui,
                                "Les pistes coupées (M) ou hors solo ne sont pas cochées : le mix correspond à ce que vous entendez.",
                            );
                        }
                        exp_row(ui, "Fichiers", |ui| {
                            ui.checkbox(&mut d.s.stems, "Pistes séparées").on_hover_text(
                                "Un fichier par piste, mono ou stéréo comme la piste : pour remixer ailleurs ou envoyer les pistes à un ingé son.",
                            );
                            ui.add_space(10.0);
                            ui.checkbox(&mut d.s.mix, "Mix").on_hover_text(
                                "Les pistes cochées mélangées dans un seul fichier : la version à faire écouter.",
                            );
                            if d.s.mix {
                                ui.add_space(4.0);
                                w::segmented_tips(
                                    ui,
                                    "exp_mixch",
                                    &mut d.s.mix_mono,
                                    &[
                                        (false, "Stéréo", "Le mix normal, en deux canaux gauche et droite."),
                                        (
                                            true,
                                            "Mono",
                                            "Un seul canal : diffusion sur une seule enceinte, vérification de la compatibilité mono.",
                                        ),
                                    ],
                                );
                            }
                        });
                        exp_row(ui, "Plage", |ui| {
                            let mut opts = vec![(
                                RangeChoice::All,
                                all_label.as_str(),
                                "Du début de la session jusqu'à la fin du dernier clip.",
                            )];
                            if let Some(l) = &sel_label {
                                opts.push((
                                    RangeChoice::Selection,
                                    l.as_str(),
                                    "Seulement entre les repères In et Out posés sur la règle : un extrait, un seul morceau du concert.",
                                ));
                            }
                            w::segmented_tips(ui, "exp_range", &mut d.range, &opts);
                            if sel_label.is_none() {
                                ui.add_space(6.0);
                                ui.label(
                                    egui::RichText::new(
                                        "Plage In/Out : Ctrl+clic et Alt+clic sur la règle",
                                    )
                                    .font(t::regular(12.0))
                                    .color(t::TEXT_3),
                                );
                            }
                        });

                        // ------------------------------------------------ Format
                        Self::section(ui, "Format");
                        exp_row(ui, "Format", |ui| {
                            if w::segmented_tips(ui, "exp_fmt", &mut d.s.format, &EXPORT_FORMATS) {
                                d.s.fit_format();
                            }
                        });
                        let lossy = d.s.format.is_lossy();
                        match d.s.format {
                            ExportFormat::Mp3 => exp_row(ui, "Débit", |ui| {
                                let opts: Vec<(Mp3Bitrate, &str, &str)> = Mp3Bitrate::ALL
                                    .iter()
                                    .map(|&b| (b, b.label(), mp3_tip(b)))
                                    .collect();
                                w::segmented_tips(ui, "exp_mp3", &mut d.s.mp3_bitrate, &opts);
                            }),
                            ExportFormat::Ogg => exp_row(ui, "Qualité", |ui| {
                                let opts: Vec<(OggQuality, &str, &str)> = OggQuality::ALL
                                    .iter()
                                    .map(|&q| (q, q.label(), ogg_tip(q)))
                                    .collect();
                                w::segmented_tips(ui, "exp_ogg", &mut d.s.ogg_quality, &opts);
                            }),
                            _ => exp_row(ui, "Résolution", |ui| {
                                let opts: Vec<(BitDepth, &str, &str)> = d
                                    .s
                                    .format
                                    .depths()
                                    .iter()
                                    .map(|&b| (b, b.label(), depth_tip(b)))
                                    .collect();
                                w::segmented_tips(ui, "exp_depth", &mut d.s.depth, &opts);
                                if d.s.output_depth() == Some(BitDepth::I16) {
                                    ui.add_space(10.0);
                                    ui.checkbox(&mut d.s.dither, "Dither").on_hover_text(
                                        "Un bruit infime ajouté au passage en 16 bits : évite le grain dans les fins de morceau et les fondus. À laisser coché.",
                                    );
                                }
                            }),
                        }
                        exp_row(ui, "Fréquence", |ui| {
                            let rates: Vec<(u32, String)> = SAMPLE_RATES
                                .iter()
                                .copied()
                                .filter(|&r| !lossy || r <= 48000)
                                .map(|r| (r, format!("{} kHz", fmt_khz(r).replace('.', ","))))
                                .collect();
                            let opts: Vec<(u32, &str, &str)> = rates
                                .iter()
                                .map(|(r, l)| (*r, l.as_str(), rate_tip(*r)))
                                .collect();
                            w::segmented_tips(ui, "exp_rate", &mut d.s.sample_rate, &opts);
                        });
                        if d.s.format == ExportFormat::Bwf
                            && d.s.output_depth() == Some(BitDepth::F32)
                        {
                            exp_hint(
                                ui,
                                "BWF en 32 bits flottant : certains outils de diffusion n'acceptent que le BWF en 16 ou 24 bits.",
                            );
                        }
                        if d.s.output_rate() != sr {
                            exp_hint(
                                ui,
                                &format!(
                                    "Session enregistrée en {} kHz : conversion de fréquence de haute qualité à l'export.",
                                    fmt_khz(sr).replace('.', ",")
                                ),
                            );
                        }
                        if lossy {
                            exp_hint(
                                ui,
                                "MP3 et Ogg s'arrêtent à 48 kHz. Pour archiver ou retravailler, préférez WAV ou FLAC.",
                            );
                        }

                        // ------------------------------------------------ Volume
                        Self::section(ui, "Volume");
                        exp_row(ui, "Normalisation", |ui| {
                            w::segmented_tips(ui, "exp_norm", &mut d.s.normalize, &NORMALIZE_OPTS);
                        });
                        if !d.s.normalize.is_off() {
                            exp_hint(
                                ui,
                                if matches!(d.s.normalize, Normalize::Lufs(_)) {
                                    "Appliquée à chaque fichier séparément. Crête vraie gardée sous −1 dBTP (le gain est réduit si besoin)."
                                } else {
                                    "Appliquée à chaque fichier séparément : chaque piste atteint la même crête."
                                },
                            );
                        }

                        // ------------------------------------------------ Destination
                        Self::section(ui, "Destination");
                        exp_row(ui, "Dossier", |ui| {
                            let mut txt = d.s.dir.display().to_string();
                            let w_edit = (ui.available_width() - 96.0).max(120.0);
                            if ui
                                .add(egui::TextEdit::singleline(&mut txt).desired_width(w_edit))
                                .on_hover_text("Dossier où les fichiers sont écrits (créé si besoin). Un fichier du même nom est remplacé.")
                                .changed()
                            {
                                d.s.dir = PathBuf::from(txt);
                            }
                            if w::secondary_button(ui, "Choisir…").clicked()
                                && let Some(p) = rfd::FileDialog::new()
                                    .set_directory(&self.project.dir)
                                    .pick_folder()
                            {
                                d.s.dir = p;
                            }
                        });
                    });

                // ------------------------------------------------ Résumé + Exporter
                ui.add_space(14.0);
                let chosen: Vec<usize> = with_audio
                    .iter()
                    .filter(|x| d.tracks.contains(&x.0))
                    .map(|x| x.3)
                    .collect();
                let mut chans: Vec<usize> = Vec::new();
                if d.s.stems {
                    chans.extend(&chosen);
                }
                if d.s.mix && !chosen.is_empty() {
                    chans.push(if d.s.mix_mono { 1 } else { 2 });
                }
                let n_files = chans.len();
                let len = match (d.range, self.selection) {
                    (RangeChoice::Selection, Some((a, b))) => b.saturating_sub(a),
                    _ => session_len,
                };
                let secs = len as f64 / sr as f64;
                let bytes: f64 = chans.iter().map(|&c| d.s.estimated_bytes(secs, c)).sum();
                let mut parts = vec![
                    format!("{n_files} fichier(s)"),
                    d.s.format.label().to_string(),
                ];
                match d.s.format {
                    ExportFormat::Mp3 => parts.push(d.s.mp3_bitrate.label().to_string()),
                    ExportFormat::Ogg => parts.push(d.s.ogg_quality.label().to_string()),
                    _ => {
                        if let Some(b) = d.s.output_depth() {
                            parts.push(b.label().to_string());
                        }
                        if d.s.dither_applies() {
                            parts.push("dither".into());
                        }
                    }
                }
                parts.push(format!("{} kHz", fmt_khz(d.s.output_rate()).replace('.', ",")));
                if let Some((_, l, _)) = NORMALIZE_OPTS
                    .iter()
                    .find(|o| o.0 == d.s.normalize && !o.0.is_off())
                {
                    parts.push((*l).to_string());
                }
                if n_files > 0 {
                    parts.push(format!("≈ {}", fmt_bytes(bytes)));
                }
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(parts.join(" · "))
                            .font(t::regular(12.0))
                            .color(t::TEXT_3),
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
            let dir = match s.dir.strip_prefix(&self.project.dir) {
                Ok(rel) => rel.to_path_buf(),
                // Dossier choisi explicitement hors de la session : gardé tel quel.
                Err(_) => s.dir.clone(),
            };
            self.export_last = Some(ExportSettings {
                tracks: Vec::new(),
                range: None,
                dir,
                ..s.clone()
            });
            match ExportJob::start(&self.project, s) {
                Ok(job) => self.export_job = Some(job),
                Err(e) => self.notify(format!("{e:#}"), true),
            }
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
                // Au lancement, aucune session n'est ouverte : rien à reprendre.
                if !self.start_screen {
                    ui.add_space(6.0);
                    ui.checkbox(
                        &mut d.keep_tracks,
                        "Reprendre les pistes et les entrées actuelles",
                    );
                }
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
        let mut dismiss = false;
        egui::Area::new(egui::Id::unique("toast"))
            .anchor(Align2::CENTER_BOTTOM, vec2(0.0, -48.0))
            .order(egui::Order::Tooltip)
            .interactable(tt.error)
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
                if tt.error
                    && ui
                        .interact(
                            r,
                            ui.make_persistent_id("toast_close"),
                            egui::Sense::click(),
                        )
                        .on_hover_text("Clic pour fermer")
                        .clicked()
                {
                    dismiss = true;
                }
            });
        if dismiss {
            self.toast = None;
        }
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
            self.complete_journal();
            self.poll_finishing(false);
            self.poll_save_as();
            self.poll_play_errors();
            self.watch_device();
            self.poll_import();
            self.soundboard_background(ctx);
            self.update_background(ctx);
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
        // Hors du garde-fou : le message d'erreur et la confirmation de fermeture doivent
        // s'afficher même si le reste de l'interface a planté.
        self.toast_ui(&ctx);
        self.confirm_close_dialog(&ctx);
        // 60 images/s quand quelque chose bouge (son, transport, animation), 10 sinon
        // (`logic` repasse toutes les 100 ms) : pas de calcul inutile à l'arrêt.
        if self.needs_animation() {
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
        }
    }

    fn on_exit(&mut self) {
        self.midi.leds_off(self.board.led_mode);
        self.stop();
        self.finish_export_for_exit();
        // Import en cours : annulé (les fichiers à moitié convertis sont supprimés).
        if let Some(job) = self.audio_import.take() {
            job.cancel.store(true, Ordering::Relaxed);
            let _ = job.join();
        }
        self.poll_finishing(true);
        // Copie en cours (« Enregistrer sous », import) : on la laisse se terminer.
        for rx in [
            self.save_as_job.take().map(|r| {
                Box::new(move || r.recv_timeout(std::time::Duration::from_secs(120)).is_ok())
                    as Box<dyn FnOnce() -> bool>
            }),
            self.import_job.take().map(|r| {
                Box::new(move || r.recv_timeout(std::time::Duration::from_secs(120)).is_ok())
                    as Box<dyn FnOnce() -> bool>
            }),
        ]
        .into_iter()
        .flatten()
        {
            let _ = rx();
        }
        self.autosave();
        self.board_changed = Some(0.0);
        self.save_board_now();
        // Copie de sécurité : fichiers des prises et double de la session finis d'écrire
        // sur le second disque (borné : un disque mort ne bloque pas la fermeture).
        crate::recorder::wait_backups(std::time::Duration::from_secs(15));
        self.backup_jobs = None;
        if let Some(h) = self.backup_thread.take() {
            let t0 = std::time::Instant::now();
            while !h.is_finished() && t0.elapsed() < std::time::Duration::from_secs(10) {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }
}

impl App {
    fn frame(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        // (temps, mémoire, MIDI, LED : traités dans `logic`, appelé juste avant)
        self.update_meters();
        self.global_keys(&ctx);
        self.update_title(&ctx);
        if let Some(tch) = self.board_changed
            && self.now - tch > 1.0
        {
            self.save_board_now();
        }

        // Fermeture de la fenêtre pendant une prise ou un export : on demande confirmation.
        if ctx.input(|i| i.viewport().close_requested())
            && (self.is_recording() || self.export_job.is_some())
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm_close = true;
        }
        // Sauvegarde automatique des modifications (montage, pistes…).
        if self.now - self.last_autosave > 30.0
            && !self.is_recording()
            && self.project.dirty
            && self.worth_saving()
        {
            self.last_autosave = self.now;
            if let Err(e) = self.project.save() {
                self.save_failed(e);
            }
        }

        // Accueil du lancement : page entière, rien d'autre tant qu'aucun choix n'est fait.
        if self.start_screen {
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(t::BG))
                .show(ui, |ui| self.welcome_page(ui));
            self.new_session_dialog(&ctx);
            self.browser_dialog(&ctx);
            self.settings_dialog(&ctx);
            self.update_dialog(&ctx);
            self.save_as_dialog(&ctx);
            // Fenêtre « Nouvelle session » annulée : retour à l'accueil.
            if self.start_screen && self.welcome.is_none() && !self.dialog_open() {
                self.welcome = Some(crate::ui::welcome::Welcome::with_ctx(&ctx));
            }
            return;
        }

        egui::Panel::top("top")
            .resizable(false)
            .exact_size(44.0)
            .frame(egui::Frame::new().fill(t::SURFACE))
            .show(ui, |ui| self.top_bar(ui));
        let alerts = self.live_alerts();
        if !alerts.is_empty() {
            let blink = (self.now * 2.0).fract() < 0.6;
            egui::Panel::top("alerts")
                .resizable(false)
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
            .resizable(false)
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

        self.settings_dialog(&ctx);
        self.export_dialog(&ctx);
        self.new_session_dialog(&ctx);
        self.new_tracks_dialog(&ctx);
        self.audio_import_dialog(&ctx);
        self.update_dialog(&ctx);
        self.browser_dialog(&ctx);
        self.save_as_dialog(&ctx);
    }

    /// Fermeture de l'application pendant un export : il est annulé et on attend qu'il ait
    /// supprimé ses fichiers incomplets (quelques millisecondes).
    fn finish_export_for_exit(&mut self) {
        if let Some(job) = self.export_job.take() {
            job.cancel.store(true, Ordering::Relaxed);
            let _ = job.join();
        }
    }

    fn confirm_close_dialog(&mut self, ctx: &egui::Context) {
        if !self.confirm_close {
            return;
        }
        let recording = self.is_recording();
        if !recording && self.export_job.is_none() {
            self.confirm_close = false;
            return;
        }
        let (title, text, keep_label, quit_label) = if recording {
            (
                "ENREGISTREMENT EN COURS",
                "Fermer TracksPlus arrêterait la prise. Les fichiers déjà enregistrés seraient conservés.",
                "Continuer l'enregistrement",
                "Arrêter, sauvegarder et quitter",
            )
        } else {
            (
                "EXPORT EN COURS",
                "Fermer TracksPlus annulerait l'export. Aucun fichier incomplet ne serait laissé et les exports précédents resteraient intacts.",
                "Continuer l'export",
                "Annuler l'export et quitter",
            )
        };
        let mut keep = false;
        let mut quit = false;
        egui::Modal::new(egui::Id::unique("confirm_close"))
            .frame(t::window_frame())
            .show(ctx, |ui| {
                ui.set_width(420.0);
                ui.label(
                    egui::RichText::new(title)
                        .font(t::semibold(12.0))
                        .color(t::REC),
                );
                ui.add_space(8.0);
                ui.label(text);
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    if w::primary_button(ui, None, keep_label, true).clicked() {
                        keep = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if w::danger_button(ui, quit_label).clicked() {
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
            self.finish_export_for_exit();
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

/// Texte d'une liste déroulante raccourci à sa largeur : un nom long ne l'élargit pas
/// (toutes les listes d'un dialogue gardent la même largeur, bien alignées).
fn combo_text(ui: &egui::Ui, text: &str, width: f32) -> String {
    let font = egui::TextStyle::Button.resolve(ui.style());
    w::fit_text(ui, text, &font, width - 28.0)
}

/// Liste déroulante d'une destination de sortie : toutes les paires de tous les
/// périphériques, plus un choix « hérité » (`None`) en tête de liste.
#[allow(clippy::too_many_arguments)]
fn dest_combo(
    ui: &mut egui::Ui,
    id: &str,
    width: f32,
    outs: &[DeviceInfo],
    device: &mut Option<String>,
    left: &mut u16,
    none_item: &str,
    none_text: &str,
) {
    let cur = match device.as_ref() {
        Some(name) => {
            let ch = outs
                .iter()
                .find(|o| &o.name == name)
                .map_or(2, |o| o.channels);
            out_dest_label(name, ch, *left)
        }
        None => none_text.to_string(),
    };
    egui::ComboBox::from_id_salt(id)
        .width(width)
        .height(360.0)
        .selected_text(combo_text(ui, &cur, width))
        .show_ui(ui, |ui| {
            if ui.selectable_label(device.is_none(), none_item).clicked() {
                *device = None;
                *left = 0;
            }
            for d in outs {
                for l in (0..d.channels.max(2)).step_by(2) {
                    let sel = device.as_deref() == Some(&d.name) && *left == l;
                    if ui
                        .selectable_label(sel, out_dest_label(&d.name, d.channels, l))
                        .clicked()
                    {
                        *device = Some(d.name.clone());
                        *left = l;
                    }
                }
            }
        });
}

/// Libellé d'une destination de sortie : le nom du périphérique s'il n'a que deux
/// canaux (cas WASAPI), sinon le nom et la paire (cas ASIO, Dante…).
fn out_dest_label(device: &str, channels: u16, left: u16) -> String {
    if channels <= 2 {
        device.to_string()
    } else {
        format!("{device} · Sorties {}–{}", left + 1, left + 2)
    }
}

/// Largeur de la colonne des libellés de la fenêtre d'export.
const EXP_LABEL_W: f32 = 104.0;

/// Ligne de la fenêtre d'export : libellé à gauche, réglages à droite.
fn exp_row(ui: &mut egui::Ui, label: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(vec2(EXP_LABEL_W, 24.0), egui::Sense::hover());
        ui.painter().text(
            r.left_center(),
            Align2::LEFT_CENTER,
            label,
            t::regular(12.0),
            t::TEXT_2,
        );
        add(ui);
    });
    ui.add_space(4.0);
}

/// Petite ligne d'explication sous un réglage de la fenêtre d'export.
fn exp_hint(ui: &mut egui::Ui, text: &str) {
    ui.horizontal(|ui| {
        ui.add_space(EXP_LABEL_W + ui.spacing().item_spacing.x);
        ui.label(
            egui::RichText::new(text)
                .font(t::regular(11.5))
                .color(t::TEXT_3),
        );
    });
    ui.add_space(4.0);
}

/// Formats d'export, avec leur bulle d'aide.
const EXPORT_FORMATS: [(ExportFormat, &str, &str); 6] = [
    (
        ExportFormat::Wav,
        "WAV",
        "Non compressé, lu partout : envoi à un studio, import dans un autre logiciel audio. Passe en RF64 au-delà de 4 Go.",
    ),
    (
        ExportFormat::Bwf,
        "BWF",
        "WAV avec horodatage et description : en post-production vidéo, chaque fichier se recale seul à sa position.",
    ),
    (
        ExportFormat::Aiff,
        "AIFF",
        "Non compressé, l'équivalent Apple du WAV : quand le studio qui reçoit travaille sur Mac et le demande.",
    ),
    (
        ExportFormat::Flac,
        "FLAC",
        "Sans perte, environ 2× plus léger que le WAV : archivage, envoi d'un concert entier à un mixeur.",
    ),
    (
        ExportFormat::Mp3,
        "MP3",
        "Compressé, lisible partout : écoute rapide, envoi par e-mail ou messagerie, démo à faire écouter.",
    ),
    (
        ExportFormat::Ogg,
        "Ogg Vorbis",
        "Compressé, un peu meilleur que le MP3 à débit égal : sons de jeux vidéo, applications, web.",
    ),
];

/// Normalisations proposées, avec leur bulle d'aide.
const NORMALIZE_OPTS: [(Normalize, &str, &str); 6] = [
    (
        Normalize::Off,
        "Aucune",
        "Volume d'origine, tel qu'enregistré : pour un mixeur qui veut toute la dynamique.",
    ),
    (
        Normalize::Peak(-0.1),
        "Crête −0,1 dB",
        "Le point le plus fort amené juste sous 0 dB : le plus fort possible sans rien changer au son.",
    ),
    (
        Normalize::Peak(-1.0),
        "Crête −1 dB",
        "Comme −0,1 dB avec 1 dB de marge : plus sûr avant une conversion en MP3.",
    ),
    (
        Normalize::Lufs(-14.0),
        "−14 LUFS",
        "Volume perçu des plateformes de streaming musical : le morceau ne sera ni baissé ni remonté.",
    ),
    (
        Normalize::Lufs(-16.0),
        "−16 LUFS",
        "Volume conseillé pour les podcasts et la voix parlée.",
    ),
    (
        Normalize::Lufs(-23.0),
        "−23 LUFS",
        "Norme radio et télé européenne (EBU R128) : livraison à une chaîne.",
    ),
];

fn depth_tip(b: BitDepth) -> &'static str {
    match b {
        BitDepth::I16 => "Qualité CD : fichier final très compatible, pressage CD.",
        BitDepth::I24 => {
            "Standard studio : mixage, mastering, échange entre studios. Le bon choix par défaut."
        }
        BitDepth::I32 => "Rarement demandé : seulement si un studio ou un outil l'exige.",
        BitDepth::F32 => {
            "Ne sature jamais, même au-dessus de 0 dB : fichiers de travail à retoucher ensuite."
        }
    }
}

fn rate_tip(r: u32) -> &'static str {
    match r {
        44100 => "Fréquence du CD et de la musique en ligne : livraison d'un morceau fini.",
        48000 => {
            "Norme vidéo, télé et cinéma : tout ce qui accompagne une image (clip, film, captation)."
        }
        88200 => "Haute résolution, multiple du CD : mastering qui finira en 44,1 kHz.",
        96000 => "Haute résolution studio : archivage, distribution haute résolution.",
        176400 => {
            "Très haute résolution, multiple du CD : archivage. Fichiers 4× plus lourds qu'en 44,1 kHz."
        }
        _ => "Très haute résolution : archivage. Fichiers 4× plus lourds qu'en 48 kHz.",
    }
}

fn mp3_tip(b: Mp3Bitrate) -> &'static str {
    match b {
        Mp3Bitrate::Cbr128 => "Léger : maquette envoyée par e-mail, écoute rapide sur téléphone.",
        Mp3Bitrate::Cbr192 => "Bon compromis : écoute au casque, partage en ligne.",
        Mp3Bitrate::Cbr256 => "Très bonne qualité : écoute attentive, diffusion en ligne.",
        Mp3Bitrate::Cbr320 => "Qualité MP3 maximale : version d'écoute d'un morceau fini.",
        Mp3Bitrate::V0 => "Débit variable : qualité proche du 320 kbit/s, fichier plus léger.",
    }
}

fn ogg_tip(q: OggQuality) -> &'static str {
    match q {
        OggQuality::Q5 => "Léger : sons de jeux vidéo, applications, web.",
        OggQuality::Q7 => "Très bonne qualité : musique, écoute attentive.",
        OggQuality::Q9 => "Qualité Ogg maximale : version d'écoute d'un morceau fini.",
    }
}

/// Taille lisible (« 1,2 Go », « 350 Mo »).
fn fmt_bytes(b: f64) -> String {
    if b >= 1e9 {
        format!("{:.1} Go", b / 1e9).replace('.', ",")
    } else if b >= 1e6 {
        format!("{:.0} Mo", b / 1e6)
    } else {
        "< 1 Mo".into()
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
    #[test]
    fn same_volume_detects_same_disk() {
        let t = std::env::temp_dir();
        assert!(super::same_volume(&t, &t.join("a/b/pas-encore")));
        #[cfg(windows)]
        {
            use std::path::Path;
            assert!(super::same_volume(
                Path::new(r"C:\Musique"),
                Path::new(r"c:\Secours")
            ));
            assert!(!super::same_volume(
                Path::new(r"C:\Musique"),
                Path::new(r"E:\Secours")
            ));
        }
    }

    /// Place libre mesurée sur le disque qui contiendra le dossier, même s'il n'existe pas
    /// encore (copie de sécurité jamais utilisée).
    #[test]
    fn free_space_of_future_folder() {
        let base = std::env::temp_dir();
        let missing = base.join("tp-pas-encore-cree").join("Session (1234abcd)");
        assert!(!missing.exists());
        assert!(super::free_space(&missing).is_some_and(|f| f > 0));
    }

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
