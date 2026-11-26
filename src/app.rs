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
use crate::recorder::{ErrorsGen, FinishError, Finished, RecResult, RecSpec, Recorder, Segment};
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

#[derive(Clone)]
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
    /// Pilote ASIO ouvert par le moteur à l'ouverture des réglages (les autres pilotes
    /// ASIO ne peuvent pas être interrogés pendant ce temps, voir `list_devices_in_use`).
    pub asio_in_use: Option<String>,
    /// Interroger les pilotes ASIO maintenant (`App::probe_asio`).
    pub probe: bool,
    /// Carte son choisie (`card_of`) : l'entrée et les sorties proposées sont les siennes.
    /// `None` : périphériques par défaut de Windows (hors ASIO).
    pub card: Option<String>,
}

impl SettingsDialog {
    fn new(prefs: &AudioPrefs, asio_in_use: Option<String>) -> Self {
        let mut d = Self {
            asio_in_use,
            probe: false,
            card: None,
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
        let asio = d.asio();
        d.card = d
            .prefs
            .output
            .as_ref()
            .or(d.prefs.input.as_ref())
            .map(|n| card_of(n, asio));
        d
    }

    fn asio(&self) -> bool {
        self.prefs.host.as_deref() == Some("ASIO")
    }

    /// Cartes son du pilote, dans l'ordre de la liste.
    fn cards(&self) -> Vec<String> {
        let asio = self.asio();
        let mut v: Vec<String> = Vec::new();
        for d in self.ins.iter().chain(&self.outs) {
            let c = card_of(&d.name, asio);
            if !v.contains(&c) {
                v.push(c);
            }
        }
        v
    }

    fn on_card(&self, d: &DeviceInfo) -> bool {
        self.card
            .as_ref()
            .is_none_or(|c| card_of(&d.name, self.asio()) == *c)
    }

    /// Entrées de la carte choisie (toutes sans carte choisie), dans l'ordre naturel.
    fn card_ins(&self) -> Vec<DeviceInfo> {
        natural_order(
            self.ins
                .iter()
                .filter(|d| self.on_card(d))
                .cloned()
                .collect(),
        )
    }

    /// Sorties de la carte choisie (toutes sans carte choisie), dans l'ordre naturel.
    fn card_outs(&self) -> Vec<DeviceInfo> {
        natural_order(
            self.outs
                .iter()
                .filter(|d| self.on_card(d))
                .cloned()
                .collect(),
        )
    }

    /// Choix de la carte son : entrée et sortie principale prises sur elle, soundboard et
    /// monitoring « comme le main ». Pilote ASIO pas encore interrogé : il l'est aussitôt.
    fn select_card(&mut self, card: Option<String>) {
        self.card = card;
        let (ins, outs) = (self.card_ins(), self.card_outs());
        let p = &mut self.prefs;
        p.out_left = 0;
        p.pad_output = None;
        p.pad_left = 0;
        p.monitor_output = None;
        p.monitor_left = 0;
        if self.card.is_none() {
            p.input = None;
            p.output = None;
            return;
        }
        p.input = ins.first().map(|d| d.name.clone());
        p.output = outs.first().map(|d| d.name.clone());
        self.probe |= ins.iter().chain(&outs).any(|d| d.note.is_some());
    }

    fn refresh(&mut self) {
        let (ins, outs) =
            audio::list_devices_in_use(self.prefs.host.as_deref(), self.asio_in_use.as_deref());
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
    rx: std::sync::mpsc::Receiver<Finished>,
    /// Position de la prise sur la timeline (déjà recalée).
    pub start: u64,
    /// Ce qui caractérise la prise, figé au Stop.
    take: TakeEnd,
    /// Avancement du thread d'écriture, et dernier changement observé (valeur, instant) :
    /// un disque lent qui écrit encore n'est jamais pris pour un disque mort.
    progress: Arc<std::sync::atomic::AtomicU64>,
    seen: (u64, f64),
    /// « Finalisation lente » déjà signalée.
    slow_told: bool,
}

impl Finishing {
    /// Dossier de la session de la prise.
    pub fn dir(&self) -> &Path {
        &self.take.dir
    }
}

/// Prise arrêtée, en attente d'intégration à la session : tout ce dont l'intégration a
/// besoin est figé au Stop (une autre prise peut avoir commencé entre-temps).
struct TakeEnd {
    /// Le callback a rendu les cibles à l'arrêt.
    released: bool,
    /// Formes d'onde en direct.
    live: Vec<LiveTake>,
    /// Latence aller-retour à compenser (trames).
    lat: u64,
    /// Prise arrêtée parce que la carte son a disparu.
    lost_card: bool,
    /// Prise coupée par un trou de l'entrée plus long que les tampons (la suite est relancée
    /// à sa place).
    split: bool,
    /// Silence inséré pour combler les trous de l'entrée (trames).
    gap: u64,
    /// Trou qui n'a pas pu être comblé (trames).
    gap_missing: u64,
    /// Problèmes de la copie de sécurité (au plus quelques lignes, voir `ErrLog`).
    backup_warn: Vec<String>,
    overruns: u32,
    xruns: u32,
    /// Une nouvelle prise a commencé pendant que le disque ne répondait plus : celle-ci est
    /// intégrée à son arrivée sans toucher à la prise en cours (forme d'onde en direct,
    /// tête de lecture), et sa sauvegarde se fait en arrière-plan si une autre prise tourne.
    merge: bool,
    /// Session de la prise, et son journal de secours (dernière version).
    dir: PathBuf,
    journal: Option<recovery::Journal>,
}

/// Issue de la fin d'écriture d'une prise.
enum TakeOutcome {
    Done(Finished),
    /// Thread d'écriture absent ou arrêté anormalement : récupération depuis le disque.
    Lost,
    /// Fermeture de l'application alors que le disque n'avance plus : le journal reste,
    /// la récupération se fera à la prochaine ouverture.
    StillWriting,
}

/// Copie de sécurité d'une prise dont le résultat n'est pas encore arrivé.
struct PendingBackup {
    rx: crossbeam_channel::Receiver<Result<Vec<RecResult>, FinishError>>,
    start: u64,
    lat: u64,
    /// Session de la prise.
    dir: PathBuf,
    /// Prises posées d'après le disque principal (piste, prise).
    main: Vec<(u64, Clip)>,
}

/// Piste dont la copie de sécurité a été découpée autrement que le principal (trou sur un
/// seul des deux disques) : dans le double de la session, les prises `main` sont
/// remplacées par `backup`, qui référencent les fichiers réellement présents sur le
/// second disque.
#[derive(Clone, Debug)]
struct BackupAlt {
    dir: PathBuf,
    track_id: u64,
    main: Vec<Clip>,
    backup: Vec<Clip>,
}

/// Fin de prise trop lente : signalée à l'opérateur.
const FINISH_SLOW_SECS: f64 = 10.0;
/// Plus aucun progrès de l'écriture depuis ce délai : une nouvelle prise peut commencer
/// (la précédente sera complétée dès que son disque répondra).
const FINISH_STALLED_SECS: f64 = 30.0;
/// Prises bloquées gardées en attente au plus (chacune retient ses tampons).
const MAX_STALLED_TAKES: usize = 3;
/// Fichiers inutiles en attente de suppression gardés au plus (une longue journée de prises
/// sur une session dont la sauvegarde échoue ne fait pas grossir la liste sans fin).
const MAX_STALE_FILES: usize = 4096;

/// Sauvegarde en arrière-plan suivie : (session, résultat, fichiers à supprimer si elle
/// réussit).
type RecSave = (
    PathBuf,
    std::sync::mpsc::Receiver<anyhow::Result<()>>,
    Vec<PathBuf>,
);

/// Départ d'une prise pendant la fin d'écriture de la précédente.
#[derive(Debug, PartialEq, Eq)]
enum OverFinishing {
    /// Rien en cours : départ normal.
    Free,
    /// La précédente écrit encore (disque lent qui avance) : on attend.
    Wait,
    /// Trop de prises déjà bloquées : refus.
    TooMany,
    /// Disque sans aucun progrès depuis longtemps : la prise part quand même, la
    /// précédente sera intégrée dès que son écriture se termine.
    Adopt,
}

fn over_finishing(finishing: bool, stalled: bool, n_stalled: usize) -> OverFinishing {
    if !finishing {
        OverFinishing::Free
    } else if !stalled {
        OverFinishing::Wait
    } else if n_stalled >= MAX_STALLED_TAKES {
        OverFinishing::TooMany
    } else {
        OverFinishing::Adopt
    }
}

/// Dossier Audio d'une prise préparé dans un thread : créé, journal de secours lu (s'il
/// existe), noms des fichiers déjà présents (minuscules). `None` si le disque n'a pas
/// répondu dans `timeout` : l'interface ne se fige jamais sur un disque bloqué (le
/// thread est abandonné à son sort).
type AudioDirProbe = Result<(Option<String>, HashSet<String>), String>;

fn probe_audio_dir(
    audio: PathBuf,
    journal: PathBuf,
    timeout: std::time::Duration,
) -> Option<AudioDirProbe> {
    fn prepare(audio: &Path, journal: &Path) -> AudioDirProbe {
        std::fs::create_dir_all(audio)
            .map_err(|e| format!("Impossible de créer {} : {e}", audio.display()))?;
        let journal = std::fs::read_to_string(journal).ok();
        let names = std::fs::read_dir(audio)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().to_lowercase())
                    .collect()
            })
            .unwrap_or_default();
        Ok((journal, names))
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let (a, j) = (audio.clone(), journal.clone());
    match std::thread::Builder::new()
        .name("tracksplus-take-dir".into())
        .spawn(move || {
            let _ = tx.send(prepare(&a, &j));
        }) {
        Ok(_) => rx.recv_timeout(timeout).ok(),
        // Pas de thread possible : fait ici.
        Err(_) => Some(prepare(&audio, &journal)),
    }
}

/// Journaux de prises (sans les prises en attente qu'ils portent) appartenant à `dir`.
fn journals_of<'a>(takes: impl Iterator<Item = &'a TakeEnd>, dir: &Path) -> Vec<recovery::Journal> {
    takes
        .filter(|t| t.dir == dir)
        .filter_map(|t| t.journal.clone())
        .map(|mut j| {
            j.extra.clear();
            j
        })
        .collect()
}

/// Journal qui ne garde que les prises encore en cours d'écriture (`None` : plus rien).
fn remaining_journal(
    inflight: Vec<recovery::Journal>,
    sample_rate: u32,
) -> Option<recovery::Journal> {
    (!inflight.is_empty()).then(|| recovery::Journal {
        start: 0,
        play_offset: 0,
        latency: 0,
        sample_rate,
        takes: Vec::new(),
        extra: inflight,
    })
}

/// Prise déjà dans la session (réintégrée depuis le disque après un crash, ou à la
/// réouverture de la session pendant son écriture) : recalée sur la longueur réelle de son
/// fichier (`frames`), prolongée si elle allait jusqu'au bout. Rend les prises touchées
/// (aucune : le fichier n'est pas dans la session, la prise est à ajouter).
fn extend_existing(project: &mut Project, file: &str, frames: u64) -> Vec<u64> {
    let mut ids = Vec::new();
    for tr in &mut project.tracks {
        for c in tr.clips.iter_mut().filter(|c| c.file == file) {
            let old = c.src_len;
            c.src_len = frames;
            if c.offset + c.len >= old || c.offset + c.len > frames {
                c.len = frames.saturating_sub(c.offset).max(1);
                c.clamp_fades();
            }
            ids.push(c.id);
        }
    }
    ids
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
    /// Messages entiers, du plus ancien au plus récent (affichés l'un sous l'autre).
    items: Vec<(Level, String)>,
    error: bool,
    until: f64,
}

/// Délai minimal entre deux nouveaux rendus de la lecture quand on bouge un fader (s).
const MIX_SWITCH_EVERY: f64 = 0.12;

/// Lignes du journal des incidents affichées au plus (les plus récentes).
const INCIDENTS_SHOWN: usize = 500;

/// Durée d'affichage d'un message selon son importance (s).
fn toast_secs(level: Level) -> f64 {
    match level {
        Level::Info => 3.5,
        Level::Device => 6.0,
        Level::Warn => 8.0,
        Level::Critical => 15.0,
    }
}

/// Importance d'un message : quand il y en a trop, les moins importants partent d'abord.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Info,
    /// État de la carte son (répété à chaque situation).
    Device,
    Warn,
    /// Prise interrompue, coupée ou relancée : jamais retiré avant d'avoir été lu.
    Critical,
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
    /// Soundboard de la session (copié avec ses sons, chemins vers la copie).
    board: Board,
}

/// Placement d'une prise terminée : une prise (clip) par fichier, avec son fichier, et les
/// fichiers sans rien à poser (vides, ou entièrement avant la latence) à supprimer une
/// fois la session sauvegardée.
struct TakeLayout {
    clips: Vec<(u64, Clip, Segment)>,
    discard: Vec<String>,
}

/// Prises d'une prise terminée, recalées de la latence `lat` (le début de la prise, qui
/// tombe avant la latence, est sauté) et posées à partir de `start` : chaque fichier à sa
/// position dans la prise (bout à bout, ou avec un trou après une erreur d'écriture).
fn layout_take(
    results: &[RecResult],
    start: u64,
    lat: u64,
    alloc: &mut dyn FnMut() -> u64,
) -> TakeLayout {
    let mut out = TakeLayout {
        clips: Vec::new(),
        discard: Vec::new(),
    };
    for r in results {
        let take_len = r.frames();
        for seg in &r.segments {
            // Fichier vide, ou entièrement avant la latence (morceau d'avant le début,
            // coupé par une erreur) : rien à poser.
            if seg.frames == 0 || (seg.start + seg.frames <= lat && take_len > lat) {
                out.discard.push(seg.file.clone());
                continue;
            }
            let skip = lat
                .saturating_sub(seg.start)
                .min(seg.frames.saturating_sub(project::MIN_CLIP));
            let pos = (start + seg.start + skip).saturating_sub(lat).max(start);
            out.clips.push((
                r.track_id,
                Clip {
                    id: alloc(),
                    file: seg.file.clone(),
                    start: pos,
                    offset: skip,
                    len: seg.frames - skip,
                    src_len: seg.frames,
                    fade_in: 0,
                    fade_out: 0,
                },
                seg.clone(),
            ));
        }
    }
    out
}

/// Pistes dont la copie de sécurité n'a pas les mêmes fichiers (ou longueurs) que le
/// principal : (piste, prises du principal, prises de la copie).
fn backup_divergence(
    main: &[(u64, Clip)],
    backup: &[(u64, Clip)],
) -> Vec<(u64, Vec<Clip>, Vec<Clip>)> {
    let shape = |c: &Clip| (c.file.clone(), c.start, c.offset, c.len, c.src_len);
    let mut tracks: Vec<u64> = main.iter().chain(backup).map(|(t, _)| *t).collect();
    tracks.sort_unstable();
    tracks.dedup();
    let mut out = Vec::new();
    for t in tracks {
        let m: Vec<Clip> = main
            .iter()
            .filter(|(x, _)| *x == t)
            .map(|(_, c)| c.clone())
            .collect();
        let b: Vec<Clip> = backup
            .iter()
            .filter(|(x, _)| *x == t)
            .map(|(_, c)| c.clone())
            .collect();
        if m.iter().map(shape).ne(b.iter().map(shape)) {
            out.push((t, m, b));
        }
    }
    out
}

/// Double de la session sur le second disque : les prises du principal dont la copie a
/// été découpée autrement sont remplacées par celles de la copie (déplacées comme elles,
/// si elles ont été déplacées depuis ; laissées telles quelles si elles ont été
/// retouchées : la copie ne peut plus les suivre).
fn apply_backup_alts(project: &mut Project, alts: &[BackupAlt]) {
    for alt in alts {
        let Some(tr) = project.track_mut(alt.track_id) else {
            continue;
        };
        if alt.main.is_empty() {
            // Rien sur le disque principal pour cette piste : les fichiers de la copie.
            for c in &alt.backup {
                if !tr.clips.iter().any(|x| x.file == c.file) {
                    tr.clips.push(c.clone());
                }
            }
            continue;
        }
        let mut delta: Option<i128> = None;
        let mut same = true;
        for m in &alt.main {
            match tr.clips.iter().find(|c| c.id == m.id) {
                Some(c) if c.file == m.file && c.offset == m.offset && c.len == m.len => {
                    let d = c.start as i128 - m.start as i128;
                    if delta.is_some_and(|x| x != d) {
                        same = false;
                    }
                    delta = Some(d);
                }
                _ => same = false,
            }
        }
        if !same {
            continue;
        }
        let d = delta.unwrap_or(0);
        tr.clips.retain(|c| !alt.main.iter().any(|m| m.id == c.id));
        for b in &alt.backup {
            let mut c = b.clone();
            c.start = (c.start as i128 + d).clamp(0, project::MAX_POS as i128) as u64;
            tr.clips.push(c);
        }
    }
}

/// Quelques messages joints, puis le nombre des autres (une notification reste lisible).
fn join_few(msgs: &[String], n: usize) -> String {
    let mut s = msgs.iter().take(n).cloned().collect::<Vec<_>>().join(" ; ");
    if msgs.len() > n {
        s += &format!(" (+{} autre(s))", msgs.len() - n);
    }
    s
}

/// Soundboard dans le double de la session : les sons du dossier « Sons » de la session
/// sont copiés (seulement les nouveaux ou modifiés) et les pads pointent vers la copie.
fn backup_board(mut board: Board, from: &Path, to: &Path) {
    let sounds = crate::soundboard::SOUNDS_DIR;
    let (src, dst) = (from.join(sounds), to.join(sounds));
    if let Ok(rd) = std::fs::read_dir(&src) {
        let _ = std::fs::create_dir_all(&dst);
        for e in rd.flatten() {
            let (a, b) = (e.path(), dst.join(e.file_name()));
            // Déjà copié et pas modifié depuis (taille et date).
            let same = match (std::fs::metadata(&a), std::fs::metadata(&b)) {
                (Ok(ma), Ok(mb)) => {
                    ma.len() == mb.len()
                        && matches!((ma.modified(), mb.modified()), (Ok(x), Ok(y)) if x <= y)
                }
                _ => false,
            };
            if a.is_file() && !same {
                let _ = project::copy_synced(&a, &b);
            }
        }
    }
    for page in &mut board.pages {
        for pad in page.pads.iter_mut().flatten() {
            // Vers la copie seulement si le son y est bien (sinon l'original, plutôt
            // qu'un pad muet).
            if let Ok(rel) = pad.path.strip_prefix(from)
                && to.join(rel).is_file()
            {
                pad.path = to.join(rel);
            }
        }
    }
    let _ = board.save_session(to);
}

/// Dossier du double de la session `dir` / `name` dans la copie de sécurité `root` (voir
/// `App::backup_session_dir`).
fn backup_session_dir_for(root: &Path, dir: &Path, name: &str) -> Option<PathBuf> {
    // Comparaison sans accès disque (le second disque peut être en veille) ; casse et
    // séparateurs ignorés comme sous Windows.
    let norm = |p: &Path| {
        p.to_string_lossy()
            .replace('\\', "/")
            .trim_end_matches('/')
            .to_lowercase()
    };
    let (r, d) = (norm(root), norm(dir));
    let inside = |a: &str, b: &str| a == b || a.starts_with(&format!("{b}/"));
    if inside(&d, &r) || inside(&r, &d) {
        return None;
    }
    // Session ouverte sur le disque de la copie (Windows : comparaison des lecteurs,
    // sans accès disque) : pas de copie sur le même disque.
    #[cfg(windows)]
    if same_volume(root, dir) {
        return None;
    }
    // FNV-1a : code stable d'une version à l'autre de l'application.
    let code = d.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ b as u64).wrapping_mul(0x0100_0000_01b3)
    });
    Some(root.join(format!("{} ({:08x})", project::sanitize(name), code as u32)))
}

/// État d'une mesure d'espace libre en arrière-plan.
enum DiskPoll {
    Idle,
    Busy,
    /// Pas de réponse depuis plus de 10 s : disque bloqué.
    Stuck,
    Done(Option<u64>),
}

/// Prise arrêtée par une perte de la carte son, à relancer dès son retour.
pub struct Resume {
    /// Position sur la timeline de la fin de ce qui a été capté.
    pos: u64,
    /// Instant (horloge de l'interface) de la dernière trame reçue : le trou se mesure
    /// depuis là, la prise relancée tombe à l'heure réelle.
    at: f64,
    /// Session de la prise (une autre session ouverte entre-temps : pas de relance).
    dir: PathBuf,
    /// Battement de cœur de l'entrée vu une première fois après son retour : on attend
    /// qu'elle délivre vraiment des trames avant de relancer.
    hb_seen: Option<u64>,
}

/// Ajoute un message entier à ceux affichés (un message déjà présent n'est pas répété),
/// sans dépasser une longueur lisible : partent d'abord les informations, puis les états
/// de la carte, puis les avertissements, les plus anciens en premier. Le plus récent et
/// les messages de prise interrompue ne partent jamais.
fn push_toast(items: &mut Vec<(Level, String)>, level: Level, text: String) {
    const MAX: usize = 900;
    if let Some(i) = items.iter().position(|(_, t)| *t == text) {
        let (l, t) = items.remove(i);
        items.push((l.max(level), t));
        return;
    }
    items.push((level, text));
    let len =
        |v: &Vec<(Level, String)>| v.iter().map(|(_, t)| t.chars().count() + 1).sum::<usize>();
    while len(items) > MAX {
        let last = items.len() - 1;
        let victim = items[..last]
            .iter()
            .enumerate()
            .filter(|(_, (l, _))| *l < Level::Critical)
            .min_by_key(|(i, (l, _))| (*l, *i))
            .map(|(i, _)| i);
        match victim {
            Some(i) => {
                items.remove(i);
            }
            None => break,
        }
    }
}

/// Position de la prise relancée : fin de ce qui a été capté + durée du trou à l'heure
/// réelle, pour que la suite reste à sa place.
fn resume_position(pos: u64, gap_secs: f64, sr: u32) -> u64 {
    pos + (gap_secs.max(0.0) * sr as f64).round() as u64
}

/// Durée d'un trou, lisible (« 4,2 s », « 2 min 05 »).
fn fmt_gap(secs: f64) -> String {
    if secs < 60.0 {
        format!("{secs:.1} s").replace('.', ",")
    } else {
        fmt_duration(secs as u64)
    }
}

/// Relance automatique abandonnée si la carte n'est pas revenue dans ce délai (s) : une
/// carte rallumée des heures plus tard ne lance jamais une prise toute seule.
const RESUME_MAX: f64 = 180.0;
/// Flux reperdu moins longtemps que ça après sa réouverture (s) : les essais suivants
/// restent espacés (horloge Dante absente : ouverture acceptée mais aucun callback).
const REOPEN_FLAP: f64 = 20.0;
/// Essai de réouverture sans réponse depuis plus longtemps que ça (s) : pilote bloqué,
/// l'essai est isolé et les autres flux peuvent être rouverts sans lui.
const REOPEN_STUCK: f64 = 15.0;

/// Étape de la relance automatique d'une prise (voir `App::resume_take`).
#[derive(Debug, PartialEq)]
enum ResumeStep {
    /// L'opérateur a repris la main (autre session, réglage coupé, transport) : oubliée.
    Cancel,
    /// La carte n'est pas revenue à temps : abandonnée, avec un message.
    Expire,
    /// Le disque ne répond plus depuis plusieurs prises (`record` refuserait) : abandonnée,
    /// avec un message.
    Blocked,
    /// Attendre (prise précédente en finalisation, entrée pas encore revenue…).
    Wait,
    /// Entrée revenue : noter son battement de cœur, relancer dès qu'il avance.
    Arm(u64),
    /// Relancer maintenant.
    Go,
}

/// Ce que regarde la relance automatique.
struct ResumeCtx {
    /// Secondes depuis la fin de ce qui a été capté.
    age: f64,
    same_session: bool,
    enabled: bool,
    disk_stalled: bool,
    /// Transport à l'arrêt (rien lancé par l'opérateur).
    transport_stopped: bool,
    /// Rien en cours, finalisation de la prise précédente comprise.
    idle: bool,
    /// Entrée ouverte, ni perdue ni en cours de réouverture.
    input_ready: bool,
    beat: u64,
    hb_seen: Option<u64>,
}

fn resume_step(c: &ResumeCtx) -> ResumeStep {
    if !c.same_session || !c.enabled || !c.transport_stopped {
        return ResumeStep::Cancel;
    }
    if c.disk_stalled {
        return ResumeStep::Blocked;
    }
    if c.age > RESUME_MAX {
        return ResumeStep::Expire;
    }
    if !c.idle || !c.input_ready {
        return ResumeStep::Wait;
    }
    match c.hb_seen {
        None => ResumeStep::Arm(c.beat),
        Some(seen) if c.beat > seen => ResumeStep::Go,
        Some(_) => ResumeStep::Wait,
    }
}

/// Un passage du chien de garde des flux. `hb[slot]` = (dernier battement vu, instant où
/// il a changé), tenu à jour ici ; `beats` = battements actuels, `open` = flux ouverts,
/// `jumped` = saut dans le temps (veille, interface figée : rien n'est jugé), `rec` = prise
/// en cours (l'entrée a alors droit à `REC_STALL_STOP`). Rend (flux muets depuis trop
/// longtemps, flux ouverts qui ont battu depuis le passage précédent).
fn watchdog(
    hb: &mut [(u64, f64); audio::SLOTS],
    beats: [u64; audio::SLOTS],
    open: u8,
    now: f64,
    jumped: bool,
    rec: bool,
) -> (u8, u8) {
    let (mut stalled, mut beating) = (0, 0);
    for (slot, h) in hb.iter_mut().enumerate() {
        let bit = audio::slot_bit(slot);
        let beat = beats[slot];
        if open & bit != 0 && beat != h.0 {
            beating |= bit;
        }
        if open & bit == 0 || jumped || beat != h.0 {
            *h = (beat, now);
            continue;
        }
        let limit = if slot == audio::SLOT_INPUT && rec {
            REC_STALL_STOP
        } else {
            STREAM_STALL
        };
        if now - h.1 > limit {
            stalled |= bit;
        }
    }
    (stalled, beating)
}

/// Ce qui est connu du moteur pour choisir les flux à rouvrir (hors ASIO).
struct ReopenCtx {
    recording: bool,
    stopped: bool,
    pads_playing: bool,
    /// Flux dont le périphérique est présent mais refuse de s'ouvrir.
    refused: u8,
}

/// Flux à rouvrir maintenant parmi `need` (hors ASIO). `expand` = `Engine::expand`.
fn reopen_slots(need: u8, c: &ReopenCtx, expand: impl Fn(u8) -> (u8, u8, u8)) -> u8 {
    let mut slots = need;
    // Entrée perdue pendant une prise : la prise vient d'être arrêtée (`watch_take`).
    if c.recording {
        slots &= !audio::LOST_INPUT;
    }
    // Pads joués sur le main faute de leur sortie, et cette sortie revenue : le main est
    // rouvert lui aussi, donc seulement à l'arrêt et sans pad en lecture. (Sortie soundboard
    // présente mais qui refuse de s'ouvrir : à corriger dans les réglages, le main n'est
    // pas coupé à chaque essai pour rien.)
    let (_, _, gated) = expand(slots);
    if gated != 0 && (!c.stopped || c.pads_playing || c.refused & audio::LOST_PADS != 0) {
        slots &= !audio::LOST_PADS;
    }
    slots
}

/// Essai de réouverture en cours dans un thread.
pub struct ReconnectJob {
    since: f64,
    rx: std::sync::mpsc::Receiver<audio::Reopened>,
    /// Flux retirés du moteur par cet essai.
    slots: u8,
    /// Alerte « pilote qui ne répond plus » déjà donnée.
    alerted: bool,
}

/// Flux muet depuis plus longtemps que ça (s) : considéré comme perdu et rouvert.
const STREAM_STALL: f64 = 3.0;

/// ASIO : pilote ouvert qui n'a toujours pas démarré ce temps-là après l'alerte (s) :
/// relâché, puis réessayé plus tard (`App::watch_streams`).
const NOT_STARTED_RELEASE: f64 = 2.0;

/// Pendant une prise, l'entrée muette sans erreur du pilote (horloge Dante perdue) garde
/// la prise ouverte (alerte rouge, silence comblé à la reprise) ; arrêt seulement au-delà.
const REC_STALL_STOP: f64 = 20.0;
/// Délai maximal entre deux essais de réouverture (s) ; ASIO, sur le thread principal,
/// réessaie moins souvent (chaque essai charge les pilotes).
const RECONNECT_MAX: f64 = 10.0;
const RECONNECT_MAX_ASIO: f64 = 30.0;

/// Délai de l'essai suivant (de plus en plus espacés : l'interface ne saccade pas tant que
/// la carte est absente).
fn next_delay(delay: f64, asio: bool) -> f64 {
    (delay * 2.0).min(if asio {
        RECONNECT_MAX_ASIO
    } else {
        RECONNECT_MAX
    })
}

/// `a` et `b` sont sur le même disque (volume). Windows : même lecteur ou même partage
/// réseau ; ailleurs : même périphérique de stockage.
pub fn same_volume(a: &Path, b: &Path) -> bool {
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        // Formes longues (`\\?\C:`, `\\?\UNC\…`) ramenées aux formes courtes.
        let vol = |p: &Path| match p.components().next() {
            Some(Component::Prefix(x)) => Some(match x.kind() {
                Prefix::Disk(d) | Prefix::VerbatimDisk(d) => {
                    format!("disque:{}", (d as char).to_ascii_lowercase())
                }
                Prefix::UNC(h, sh) | Prefix::VerbatimUNC(h, sh) => format!(
                    "partage:{}\\{}",
                    h.to_string_lossy().to_lowercase(),
                    sh.to_string_lossy().to_lowercase()
                ),
                _ => x.as_os_str().to_string_lossy().to_lowercase(),
            }),
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

const OUTPUT_STALLED: &str = "Sortie audio bloquée · voir Réglages audio";

pub struct App {
    pub tab: usize,
    pub project: Project,
    pub engine: Engine,
    pub prefs: AudioPrefs,
    pub transport: Transport,
    pub playhead: u64,
    pub player: Option<Player>,
    /// Position de la timeline entendue à la trame `play_skew` du moteur (début de la
    /// lecture, ou bascule sur un nouveau rendu), et boucle de ce rendu (`heard_at`).
    play_base: u64,
    play_skew: u64,
    play_loop: Option<(u64, u64)>,
    /// Lecture en boucle sur la plage sélectionnée (bouton boucle, touche L).
    pub loop_on: bool,
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
    /// Prises précédentes dont le disque ne répondait plus quand une nouvelle prise a
    /// commencé : portées par le journal de la nouvelle, intégrées dès que leur écriture
    /// finit.
    stalled_takes: Vec<Finishing>,
    /// Sauvegarde de la session lancée en arrière-plan (début de prise, prise intégrée
    /// pendant une autre) : (session, résultat, fichiers à supprimer si elle réussit).
    rec_save: Option<RecSave>,
    /// Fichiers devenus inutiles (positions notées, fichiers vides) d'une prise intégrée,
    /// à supprimer après la prochaine sauvegarde réussie de leur session : (session, fichier).
    stale_files: Vec<(PathBuf, PathBuf)>,
    /// Journal de la prise en cours (pour le double de la session sur le second disque).
    rec_journal: Option<recovery::Journal>,
    /// La prise en cours a une copie de sécurité.
    rec_has_backup: bool,
    /// Dernière mise à jour du double de la session pendant une prise (s).
    backup_double_at: f64,
    /// Copies de sécurité de prises terminées dont le résultat n'est pas encore arrivé.
    backup_pending: Vec<PendingBackup>,
    /// Pistes dont la copie de sécurité diffère du principal (voir `BackupAlt`).
    backup_alt: Vec<BackupAlt>,
    /// Bandeau des erreurs disque, recalculé seulement quand elles changent.
    disk_err_cache: Option<(ErrorsGen, Option<String>)>,
    /// Dernière alerte « sauvegarde impossible » (pour ne pas la répéter en boucle).
    pub save_error_at: f64,
    /// Dernier changement de session refusé faute de sauvegarde (voir `save_before_leaving`).
    pub leave_anyway_at: f64,
    /// Dossier de la copie de sécurité des prises (second disque), réglage de l'ordinateur.
    pub backup_root: Option<PathBuf>,
    /// Réglage de la copie de sécurité illisible au lancement : la copie est peut-être
    /// désactivée à tort (rappelé à chaque départ de prise).
    pub backup_prefs_error: Option<String>,
    /// Thread qui tient à jour le double de la session sur le second disque.
    pub backup_jobs: Option<std::sync::mpsc::Sender<BackupJob>>,
    pub backup_thread: Option<std::thread::JoinHandle<()>>,
    /// Carte son perdue : prochain essai de réouverture (secondes) et délai courant.
    pub reconnect: Option<(f64, f64)>,
    /// La prise en cours a été arrêtée parce que la carte son a disparu.
    pub lost_during_take: bool,
    /// Sauvegarde automatique en cours (thread).
    pub autosave_job: Option<(PathBuf, std::sync::mpsc::Receiver<anyhow::Result<()>>)>,
    /// Réouverture de flux en cours dans un thread. Un seul essai à la fois, jamais
    /// abandonné (pas de threads bloqués qui s'accumulent, jamais deux ouvertures du même
    /// périphérique).
    pub reconnect_job: Option<ReconnectJob>,
    /// Essais restés bloqués chez le pilote (plus de `REOPEN_STUCK`), toujours attendus et
    /// installés s'ils finissent : leurs flux sont exclus des essais suivants, les autres
    /// flux continuent d'être rouverts. Au plus un par flux (ensembles disjoints).
    stuck_jobs: Vec<ReconnectJob>,
    /// Pertes posées par le chien de garde (flux seulement muet) : effacées si le flux
    /// repart de lui-même, sans le rouvrir (bits `LOST_*`).
    stalled: u8,
    /// Flux rouverts qui n'ont pas encore battu : « reconnectée » n'est dit qu'après leur
    /// premier callback (une ouverture acceptée peut ne jamais tourner).
    awaiting: u8,
    /// Reconnexion à annoncer dès que tout tourne.
    announce: bool,
    /// Flux ouverts vus au dernier passage, et flux qui ont battu au moins une fois depuis
    /// leur ouverture (`watch_streams`). Un flux ouvert qui n'a jamais démarré (Dante
    /// Virtual Soundcard sans horloge sur le réseau) n'est pas « perdu » : on l'attend.
    open_seen: u8,
    started: u8,
    /// Flux ouverts qui attendent leur premier bloc depuis plus de `STREAM_STALL`, et
    /// depuis quand.
    not_started: u8,
    not_started_since: f64,
    /// Pilote ASIO relâché faute de démarrer (Dante Virtual Soundcard sans horloge) :
    /// réessayé de temps en temps, sans le garder (il resterait verrouillé, ses réglages
    /// grisés), et sans répéter les messages.
    clockless: bool,
    /// Dernière réouverture : instant et délai d'attente d'alors (flux reperdu juste après :
    /// les essais restent espacés).
    last_reopen: Option<(f64, f64)>,
    /// La prise en cours a été coupée par un trou de l'entrée plus long que les tampons.
    take_split: bool,
    /// Fader ou panoramique changé, pas encore appliqué à la lecture (`flush_mix`), et
    /// dernier nouveau rendu pour cette raison.
    mix_dirty: bool,
    mix_switch_at: f64,
    /// Silence inséré dans la prise en cours déjà repéré (trames, voir `watch_take`), et
    /// nombre de trous déjà repérés.
    gap_seen: u64,
    gap_count_seen: u64,
    /// Prises abandonnées à la fermeture, disque bloqué (session, journal) : gardées dans
    /// le journal de leur session jusqu'à la prochaine ouverture.
    given_up: Vec<(PathBuf, recovery::Journal)>,
    /// Interruption du signal d'entrée en cours déjà notée dans le journal des incidents.
    stall_logged: bool,
    /// Chien de garde des flux (`audio::SLOT_*`) : dernier battement de cœur vu et instant
    /// où il a changé ; dernier passage (un saut = veille ou interface figée).
    hb: [(u64, f64); audio::SLOTS],
    hb_tick: f64,
    /// Prise arrêtée par une perte de la carte, à relancer dès son retour.
    pub resume: Option<Resume>,
    /// Réouverture ASIO en attente de la fin des pads / du transport : déjà signalée.
    reconnect_wait: bool,
    /// Dernier état de la carte signalé (pas de répétition à chaque essai).
    device_note: String,
    /// Mesure de l'espace libre en cours (thread), disque de la session puis second disque :
    /// (début, dossier, résultat). Une seule à la fois par disque, jamais abandonnée tant
    /// que la session ne change pas (un disque bloqué n'accumule pas de threads).
    pub disk_job: Option<(f64, PathBuf, std::sync::mpsc::Receiver<Option<u64>>)>,
    pub backup_disk_job: Option<(f64, PathBuf, std::sync::mpsc::Receiver<Option<u64>>)>,
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
    /// Fenêtre du journal des incidents ouverte (lignes, les plus récentes d'abord).
    pub incidents_dlg: Option<Vec<String>>,
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
    /// Dernière préparation des sauvegardes de fin de session (`None` : aucune en place).
    pending_saves_at: Option<f64>,
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
    /// Compteur d'identifiants à cet instant (voir `project::merge_clips`).
    next_id: u64,
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
        // Un flux qui ne s'ouvre pas n'empêche jamais les autres : une carte absente au
        // démarrage (rack Dante encore éteint) est reprise dès qu'elle apparaît
        // (`watch_device`) ; un réglage refusé ouvre les réglages avec l'erreur.
        let (engine, err) = match Engine::open(&prefs, &prefs.pad_out()) {
            Ok(e) => {
                let err = e.open_errors();
                (e, err)
            }
            Err(e) => (Engine::offline(), Some(format!("{e:#}"))),
        };
        let (backup_root, backup_prefs_error) = match crate::sessions::load_backup_root() {
            Ok(d) => (d, None),
            Err(e) => (None, Some(e)),
        };
        // Après la lecture du réglage de la copie (qui peut encore se trouver dans ce
        // fichier) : réglages de l'accueil abîmés mis de côté, à signaler.
        let prefs_notice = crate::sessions::check_startup_prefs();
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
            play_skew: 0,
            play_loop: None,
            loop_on: false,
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
            incidents_dlg: None,
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
            stalled_takes: Vec::new(),
            rec_save: None,
            stale_files: Vec::new(),
            rec_journal: None,
            rec_has_backup: false,
            backup_double_at: 0.0,
            backup_pending: Vec::new(),
            backup_alt: Vec::new(),
            disk_err_cache: None,
            save_error_at: f64::NEG_INFINITY,
            leave_anyway_at: f64::NEG_INFINITY,
            backup_root,
            backup_prefs_error,
            backup_jobs: None,
            backup_thread: None,
            reconnect: None,
            lost_during_take: false,
            autosave_job: None,
            reconnect_job: None,
            stuck_jobs: Vec::new(),
            stalled: 0,
            awaiting: 0,
            open_seen: 0,
            started: 0,
            not_started: 0,
            not_started_since: 0.0,
            clockless: false,
            announce: false,
            last_reopen: None,
            take_split: false,
            gap_seen: 0,
            mix_dirty: false,
            mix_switch_at: f64::NEG_INFINITY,
            gap_count_seen: 0,
            given_up: Vec::new(),
            stall_logged: false,
            hb: [(0, 0.0); audio::SLOTS],
            hb_tick: 0.0,
            resume: None,
            reconnect_wait: false,
            device_note: String::new(),
            disk_job: None,
            backup_disk_job: None,
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
            pending_saves_at: None,
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
        install_crash_safety(cc);
        // Relance automatique après une erreur fatale : on revient dans la même session,
        // même si aucune prise n'était en cours.
        let relaunched = crate::shutdown::relaunched();
        let interrupted = recovery::interrupted_sessions();
        let take_interrupted = !interrupted.is_empty();
        let reopen = interrupted.first().cloned().or_else(|| {
            if relaunched {
                recovery::last_session()
            } else {
                None
            }
        });
        // Enregistrement interrompu lors de la dernière utilisation : on restaure.
        if let Some(path) = reopen {
            // Protégé : une session illisible ne doit pas empêcher l'application de démarrer
            // (sinon elle planterait à chaque lancement, le journal étant toujours là).
            let res =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| app.load_session(&path)));
            if res.is_err() {
                app.notify(
                    format!("Récupération impossible : « {} »", path.display()),
                    true,
                );
            }
            if !matches!(res, Ok(true)) {
                // Session non ouverte : l'accueil, jamais la session neuve du lancement (elle
                // ne doit rien écrire à la place de celle qui n'a pas pu s'ouvrir).
                app.welcome = Some(crate::ui::welcome::Welcome::with_ctx(&cc.egui_ctx));
                app.start_screen = true;
            }
            // D'autres sessions ont aussi une prise interrompue : on le dit (leurs prises
            // sont récupérées à leur ouverture).
            if interrupted.len() > 1 {
                let names: Vec<String> = interrupted[1..]
                    .iter()
                    .map(|p| {
                        p.file_stem()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default()
                    })
                    .collect();
                let others = format!("Autres prises interrompues : {}", join_few(&names, 3));
                // Ajouté au message de la récupération (qui reste lisible).
                let text = match app.take_toast_text() {
                    Some(t) => format!("{t} · {others}"),
                    None => others,
                };
                app.notify(text, true);
            }
        } else {
            // Écran d'accueil : aucune session tant que l'utilisateur n'a pas choisi de
            // reprendre, créer, ouvrir ou choisir une session.
            app.welcome = Some(crate::ui::welcome::Welcome::with_ctx(&cc.egui_ctx));
            app.start_screen = true;
        }
        // Un seul message s'affiche à la fois : les avertissements du lancement sont réunis.
        let mut launch_warnings: Vec<String> = Vec::new();
        if relaunched {
            // Après l'ouverture de la session (qui pose son propre message) : l'alerte reste
            // affichée jusqu'au clic et reprend le message de l'ouverture, rien n'est perdu.
            let opened = app.take_toast_text();
            launch_warnings.push(relaunch_message(take_interrupted, opened.as_deref()));
        }
        launch_warnings.extend(
            app.backup_prefs_error
                .iter()
                .map(|e| format!("Copie de sécurité inactive : {e}"))
                .chain(prefs_notice),
        );
        if !launch_warnings.is_empty() {
            // Le message de l'ouverture (prises récupérées…) est gardé en tête.
            if !relaunched && let Some(t) = app.take_toast_text() {
                launch_warnings.insert(0, t);
            }
            app.notify(launch_warnings.join(" · "), true);
        }
        if let Some(e) = err {
            let mut d = SettingsDialog::new(&app.prefs, app.asio_in_use());
            d.error = Some(e);
            app.settings_dlg = Some(d);
        }
        app
    }

    /// Message en bas de la fenêtre. Une information disparaît seule ; une erreur ou un
    /// avertissement reste affiché jusqu'au clic : on ne peut pas le manquer.
    pub fn notify(&mut self, text: impl Into<String>, error: bool) {
        self.notify_level(text, if error { Level::Warn } else { Level::Info });
    }

    /// Note un événement dans le journal des incidents de la session ouverte (dossier des
    /// réglages tant qu'aucune session n'est ouverte). Ne bloque jamais.
    pub fn incident(&self, text: impl AsRef<str>) {
        // Session protégée (lecture seule) : rien n'est écrit dans son dossier.
        let dir = if self.start_screen || self.project.read_only.is_some() {
            match dirs::config_dir() {
                Some(d) => d.join("TracksPlus"),
                None => return,
            }
        } else {
            self.project.dir.clone()
        };
        crate::incidents::log(&dir, text.as_ref());
    }

    /// Repère posé tout seul sur la timeline à l'endroit d'un incident (trou, coupure,
    /// carte perdue, relance), aussi noté dans le journal des incidents.
    fn incident_marker(&mut self, pos: u64, name: String) {
        let id = self.project.add_marker(pos);
        if let Some(m) = self.project.markers.iter_mut().find(|m| m.id == id) {
            m.name = name.clone();
        }
        self.incident(format!(
            "Repère « {name} » posé à {}",
            fmt_time(pos, self.sr())
        ));
        // Sauvegardé sous peu (sauvegarde automatique en arrière-plan), même en pleine prise.
        if self.is_recording() {
            self.last_autosave = self.last_autosave.min(self.now - 20.0);
        }
    }

    /// Texte du message affiché (ses messages réunis), retiré de l'écran : pour le reprendre
    /// dans un message plus complet.
    fn take_toast_text(&mut self) -> Option<String> {
        let t = self.toast.take()?;
        let parts: Vec<String> = t.items.into_iter().map(|(_, text)| text).collect();
        Some(parts.join(" · "))
    }

    /// Message avec son importance (voir `Level`).
    pub fn notify_level(&mut self, text: impl Into<String>, level: Level) {
        let text = text.into();
        // État de la carte, prise interrompue, coupée ou relancée : aussi dans le journal
        // des incidents de la session (le message à l'écran finit par partir).
        if matches!(level, Level::Device | Level::Critical) {
            self.incident(text.clone());
        }
        // Une alerte encore affichée n'est jamais effacée par un nouveau message (ex. « prise
        // arrêtée, carte perdue » puis « carte reconnectée ») : il s'ajoute à la suite. Les
        // messages partent seuls (plus longtemps affichés s'ils sont importants) ; tout ce
        // qui compte reste dans le journal des incidents.
        let until = self.now + toast_secs(level);
        if let Some(t) = &mut self.toast
            && t.error
            && self.now < t.until
        {
            push_toast(&mut t.items, level, text);
            t.until = t.until.max(until);
            return;
        }
        self.toast = Some(Toast {
            items: vec![(level, text)],
            error: level > Level::Info,
            until,
        });
    }

    /// Carte choisie mais qui ne joue pas encore (barre du haut) : « pas d'horloge » (Dante
    /// sans horloge), « reconnexion… » (perdue ou absente), `None` si tout tourne.
    fn card_waiting(&self) -> Option<&'static str> {
        let st = &self.engine.status;
        let dante = [&st.input, &st.output]
            .into_iter()
            .flatten()
            .any(|n| n.to_lowercase().contains("dante"));
        if self.clockless || self.not_started != 0 {
            return Some(if dante { "pas d'horloge" } else { "en attente" });
        }
        let lost = self.engine.shared.device_lost.load(Ordering::Relaxed);
        if lost != 0 || self.engine.missing() != 0 || self.reconnect_job.is_some() {
            return Some("reconnexion…");
        }
        // Ouverte, mais pas encore de premier bloc du pilote.
        if self.engine.open_mask() & !self.started != 0 {
            return Some("démarrage…");
        }
        None
    }

    /// Numéro de la première entrée de la carte ouverte, moins un : en WASAPI, Windows
    /// découpe une carte en paires (« Analog (5+6) ») et une seule s'ouvre ; ses entrées
    /// s'affichent sous leur vrai numéro (5 et 6), pas 1 et 2.
    pub fn in_base(&self) -> u16 {
        let st = &self.engine.status;
        if st.host == "ASIO" {
            return 0;
        }
        st.input.as_deref().map_or(0, audio::channel_base)
    }

    /// Pilote ASIO ouvert par le moteur (`None` hors ASIO ou carte fermée).
    pub fn asio_in_use(&self) -> Option<String> {
        let st = &self.engine.status;
        (st.host == "ASIO")
            .then(|| st.input.clone().or_else(|| st.output.clone()))
            .flatten()
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
        self.engine.pad(PadCmd::SetDebounce {
            ms: self.board.debounce_ms,
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

    /// Prise en cours, ou dont l'écriture n'est pas finie (fin de prise, prises bloquées
    /// sur un disque lent) : rien ne doit l'interrompre (arrêt de Windows, fermeture…).
    pub fn take_in_progress(&self) -> bool {
        self.is_recording() || self.finishing.is_some() || !self.stalled_takes.is_empty()
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
            || self.confirm_close
            || self.sb.confirm_delete_page.is_some()
    }

    pub fn play(&mut self) {
        if !self.is_stopped() {
            return;
        }
        if let Some(msg) = self.output_reopening() {
            self.notify(msg, true);
            return;
        }
        if !self.engine.has_output() {
            self.notify("Aucune sortie audio · voir Réglages audio", true);
            return;
        }
        // Bandes lues sans rééchantillonnage : à une autre fréquence, elles partiraient à la
        // mauvaise vitesse et à la mauvaise hauteur.
        if let Some(msg) = self.rate_mismatch() {
            self.notify(msg, true);
            return;
        }
        if self.playhead >= self.project.end() && self.project.end() > 0 {
            self.playhead = self.selection.map_or(0, |s| s.0);
        }
        // Boucle : la lecture part du début de la plage si la tête est en dehors.
        let lp = self.active_loop();
        if let Some((a, b)) = lp
            && (self.playhead < a || self.playhead >= b)
        {
            self.playhead = a;
        }
        let (player, cons) =
            Player::start(&self.project, self.playhead, self.project.audible(), lp);
        if !self.engine.start_playback(cons) {
            // Jamais « lecture » à l'écran si le moteur n'a rien reçu.
            self.notify(OUTPUT_STALLED, true);
            return;
        }
        self.player = Some(player);
        self.play_base = self.playhead;
        self.play_skew = 0;
        self.play_loop = lp;
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
                if let Some(msg) = self.rate_mismatch().or_else(|| self.output_reopening()) {
                    self.notify(msg, true);
                    self.stop();
                    return;
                }
                let lp = self.active_loop();
                let (player, cons) = Player::start(&self.project, pos, self.project.audible(), lp);
                if !self.engine.start_playback(cons) {
                    self.notify(OUTPUT_STALLED, true);
                    self.stop();
                    return;
                }
                self.player = Some(player);
                self.play_base = pos;
                self.play_skew = 0;
                self.play_loop = lp;
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
        // Position qui aurait été entendue à cette trame, et boucle du nouveau rendu
        // (jamais pendant une prise).
        let pos = self.heard_at(at);
        let lp = if matches!(self.transport, Transport::Playing { .. }) {
            self.active_loop()
        } else {
            None
        };
        let (player, cons) = Player::start_ahead(&self.project, pos, audible, lp);
        if self.engine.switch_playback(cons, at)
            && let Some(old) = self.player.replace(player)
        {
            self.retired.push((old, self.now + 5.0));
            self.play_base = pos;
            self.play_skew = at;
            self.play_loop = lp;
        }
    }

    /// Plage de la boucle, si la boucle est active et qu'une plage (assez longue) est
    /// sélectionnée.
    pub fn active_loop(&self) -> Option<(u64, u64)> {
        if !self.loop_on {
            return None;
        }
        let (a, b) = self.selection?;
        let (a, b) = (a.min(b), a.max(b));
        (b - a >= (self.project.sample_rate as u64 / 20).max(1)).then_some((a, b))
    }

    /// Position de la timeline entendue à la trame `frames` du moteur (boucle comprise).
    fn heard_at(&self, frames: u64) -> u64 {
        let x = self.play_base + frames.saturating_sub(self.play_skew);
        match self.play_loop {
            Some((a, b)) if x >= b && b > a => a + (x - b) % (b - a),
            _ => x,
        }
    }

    /// Bouton boucle / touche L.
    pub fn toggle_loop(&mut self) {
        self.loop_on = !self.loop_on;
        if self.loop_on && self.active_loop().is_none() {
            self.notify("Boucle : sélectionnez une plage", false);
        }
        // En lecture : nouveau rendu, avec ou sans boucle (voir `flush_mix`).
        self.mix_dirty = true;
    }

    /// La session a des bandes à une autre fréquence que la carte : message, sinon `None`.
    fn rate_mismatch(&self) -> Option<String> {
        let sr = self.engine.status.sample_rate;
        (self.project.has_audio() && self.project.sample_rate != sr).then(|| {
            format!(
                "Session en {} kHz, carte en {} kHz",
                fmt_khz(self.project.sample_rate),
                fmt_khz(sr)
            )
        })
    }

    /// Sortie principale en cours de réouverture (essai lancé, pas encore installé) :
    /// message, sinon `None`. Rien n'est envoyé à l'ancien flux, fermé juste après.
    fn output_reopening(&self) -> Option<String> {
        (self.engine.reopening() & audio::LOST_OUTPUT != 0)
            .then(|| "Sortie audio en réouverture…".to_string())
    }

    /// L'entrée existe mais ne délivre rien (perdue, en cours de réouverture, ou muette) :
    /// message, sinon `None`. Jamais de prise lancée sur un flux mort.
    fn input_unavailable(&self) -> Option<String> {
        let lost = self.engine.shared.device_lost.load(Ordering::Acquire);
        let reopening = self.engine.reopening() & audio::LOST_INPUT != 0;
        if lost & audio::LOST_INPUT != 0 || reopening {
            return Some("Entrée audio perdue · reconnexion…".into());
        }
        let hb = self.hb[audio::SLOT_INPUT];
        let beat = self.engine.shared.heartbeat[audio::SLOT_INPUT].load(Ordering::Relaxed);
        if self.engine.has_input() && beat == hb.0 && self.now - hb.1 > 1.0 {
            return Some("Entrée audio : plus de signal".into());
        }
        None
    }

    pub fn record(&mut self) {
        let t0 = std::time::Instant::now();
        // Le disque de la prise précédente n'avance plus depuis longtemps : on ne bloque pas
        // le spectacle. Elle n'est confiée aux prises en attente qu'une fois la nouvelle
        // vraiment partie (plus bas), et intégrée dès que son écriture se termine.
        let adopt = match over_finishing(
            self.finishing.is_some(),
            self.finish_stalled(),
            self.stalled_takes.len(),
        ) {
            OverFinishing::Free => false,
            OverFinishing::Adopt => true,
            OverFinishing::Wait => {
                self.notify("Finalisation de la prise précédente…", false);
                return;
            }
            OverFinishing::TooMany => {
                self.notify("Disque bloqué depuis plusieurs prises", true);
                return;
            }
        };
        // Session qui ne peut pas être enregistrée (lecture seule, fichier d'une autre
        // session) : la prise ne serait notée nulle part dans la session, et son journal de
        // secours serait remplacé par celui de la prise suivante. Refus avant tout (la
        // lecture en cours continue).
        if let Some(why) = self.project.save_refused() {
            self.notify(format!("Prise impossible : {why} · Enregistrer sous"), true);
            return;
        }
        if self.transport != Transport::Stopped {
            if matches!(self.transport, Transport::Playing { .. }) {
                self.stop();
            } else {
                return;
            }
        }
        if let Some(msg) = self.input_unavailable() {
            self.notify(msg, true);
            return;
        }
        if !self.engine.has_input() {
            self.notify("Aucune entrée audio · voir Réglages audio", true);
            return;
        }
        let sr = self.engine.status.sample_rate;
        if let Some(msg) = self.rate_mismatch() {
            self.notify(msg, true);
            return;
        }
        let in_ch = self.engine.status.in_channels;
        // Entrées des pistes = numéros réels de la carte ; le flux ouvert commence à `base`.
        let base = self.in_base();
        // (piste, nom, premier canal du flux, stéréo)
        let armed: Vec<(u64, String, u16, bool)> = self
            .project
            .tracks
            .iter()
            .filter(|t| t.armed && t.input_fits(base, in_ch))
            .filter_map(|t| {
                t.engine_input(base)
                    .map(|i| (t.id, t.name.clone(), i, t.stereo))
            })
            .collect();
        // Pistes armées dont l'entrée n'existe pas sur la carte (ex. Dante revenue avec
        // moins de canaux) : on enregistre les autres, mais on le signale clairement.
        let skipped: Vec<String> = self
            .project
            .tracks
            .iter()
            .filter(|t| t.armed && !t.input_fits(base, in_ch))
            .map(|t| t.name.clone())
            .collect();
        if armed.is_empty() {
            self.notify("Aucune piste armée", true);
            return;
        }

        // 1) Tampons et cibles, sans aucun accès disque : la capture commence tout de suite,
        //    les tampons (`record_buffer_secs`) absorbent le temps de préparation des
        //    fichiers et le réveil d'un disque en veille.
        let ring_secs = record_buffer_secs(sr, armed.iter().map(|a| if a.3 { 2 } else { 1 }).sum());
        // Copie de sécurité : un second tampon par piste, rempli par le callback audio
        // lui-même (même taille que le principal) — si le disque principal se bloque, la
        // copie continue. Pas de nouvelle copie tant que celle d'une prise précédente est
        // bloquée (second disque qui ne répond plus) : ses tampons ne s'empilent pas.
        let mut backup = self.backup_session_dir();
        let backup_blocked = backup.is_some() && crate::recorder::backup_busy();
        if backup_blocked {
            backup = None;
        }
        let mut targets = Vec::new();
        let mut conss = Vec::new();
        let mut copy_conss = Vec::new();
        self.live.clear();
        for &(track_id, _, input, stereo) in &armed {
            let width: u16 = if stereo { 2 } else { 1 };
            let (prod, cons) = rtrb::RingBuffer::new(sr as usize * ring_secs * width as usize);
            let peaks: Peaks = Arc::new(RwLock::new(PeakData::default()));
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
            conss.push((cons, peaks.clone()));
            self.live.push(LiveTake { track_id, peaks });
        }
        // 2) La lecture des pistes existantes démarre AVANT la prise (son pré-remplissage,
        //    borné, peut prendre un peu de temps) ; le décalage réel entre les deux est
        //    mesuré par le callback d'entrée et compensé à la fin, avec la latence
        //    entrée + sortie de la carte.
        self.project.sample_rate = sr;
        self.rec_latency = 0;
        self.rec_overdub = false;
        // Sortie en cours de réouverture : la prise passe avant (elle se fait sans la
        // lecture des pistes), et on le dit.
        if self.project.has_audio()
            && let Some(msg) = self.output_reopening()
        {
            self.notify(format!("{msg} · prise sans lecture"), true);
        }
        if self.engine.has_output() && self.project.has_audio() {
            let (player, cons) =
                Player::start(&self.project, self.playhead, self.project.audible(), None);
            if self.engine.start_playback(cons) {
                self.player = Some(player);
                self.play_base = self.playhead;
                self.play_skew = 0;
                self.play_loop = None;
                self.rec_overdub = true;
                self.rec_latency = self.engine.round_trip_latency() as u64;
            } else {
                // Sortie bloquée : la prise se fait quand même, sans la lecture des pistes
                // (donc sans compensation de latence : rien n'a été joué).
                self.notify(OUTPUT_STALLED, true);
            }
        }
        // 3) Capture.
        if let Err(e) = self.engine.start_record(targets) {
            self.engine.stop_playback();
            self.player = None;
            // Prise précédente toujours en fin d'écriture : sa forme d'onde reste affichée.
            self.live = self
                .finishing
                .as_ref()
                .map(|f| f.take.live.clone())
                .unwrap_or_default();
            self.rec_overdub = false;
            self.rec_latency = 0;
            self.notify(format!("{e:#}"), true);
            return;
        }

        // 4) Disque : dossier, journal existant et noms des fichiers (une seule lecture du
        //    dossier), dans un thread au temps borné — un disque bloqué ne fige jamais
        //    l'interface, et la prise ne part pas sur un disque qui ne répond pas. Puis le
        //    thread d'écriture, qui écrit le journal de secours avant le premier fichier.
        //    Prises de cette session encore en cours d'écriture (disque déjà en difficulté) :
        //    attente courte.
        let inflight = self.inflight_journals();
        let timeout = if inflight.is_empty() {
            std::time::Duration::from_secs(ring_secs.saturating_sub(2).max(2) as u64)
        } else {
            std::time::Duration::from_secs(2)
        };
        let (old_journal, existing) = match probe_audio_dir(
            self.project.audio_dir(),
            recovery::journal_path(&self.project.dir),
            timeout,
        ) {
            Some(Ok(r)) => r,
            Some(Err(e)) => {
                self.abort_record_start(e);
                return;
            }
            None => {
                self.abort_record_start("Disque de la session bloqué · prise impossible".into());
                return;
            }
        };
        // Journal d'une prise précédente non intégrée (sauvegarde échouée) : récupérée avant
        // que le nouveau journal ne le remplace — sauf si des prises de cette session
        // écrivent encore (leur disque ne répond plus) : rien n'est alors lu ni réparé ici,
        // ses prises sont reportées dans le nouveau journal.
        let mut carried = Vec::new();
        // Journal d'une prise précédente non intégrée, sans prise en cours d'écriture : ses
        // prises sont intégrées plus bas, une fois l'écriture de la nouvelle prise lancée
        // (jamais de réparation ni de sauvegarde avant : les tampons de capture n'attendent
        // pas). D'ici là, elles sont portées par le nouveau journal.
        let mut late = None;
        if let Some(text) = old_journal {
            match serde_json::from_str::<recovery::Journal>(&text) {
                Ok(j) => {
                    carried = j.pending_groups(&inflight);
                    if inflight.is_empty() {
                        late = Some(j);
                    }
                }
                Err(e) => self.notify(
                    format!("Journal de la prise précédente illisible : {e}"),
                    true,
                ),
            }
        }
        let mut used = HashSet::new();
        let mut specs = Vec::new();
        for ((track_id, name, _, stereo), (cons, peaks)) in armed.into_iter().zip(conss) {
            // Nom unique sur le disque et dans cette prise, sans tenir compte de la casse.
            let file = self.project.new_take_file_in(&name, &existing, &used);
            used.insert(file.to_lowercase());
            specs.push(RecSpec {
                track_id,
                path: self.project.audio_dir().join(&file),
                file,
                peaks,
                cons,
                channels: if stereo { 2 } else { 1 },
            });
        }
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
            // Prises précédentes encore en attente (disque bloqué), et celles d'un journal
            // pas encore intégré : toujours récupérables après un crash, même si ce journal
            // remplace le leur.
            extra: inflight.into_iter().chain(carried).collect(),
        };
        let has_backup = backup.is_some();
        let recorder = match Recorder::start_with_backup(
            specs,
            sr,
            self.project.rec_format,
            backup.map(|d| (d.join(project::AUDIO_DIR), copy_conss)),
            Some((self.project.dir.clone(), journal.clone())),
        ) {
            Ok(r) => r,
            Err(e) => {
                self.abort_record_start(format!("Écriture disque impossible : {e}"));
                return;
            }
        };
        // La prise est vraiment partie : la précédente, bloquée, rejoint les prises en
        // attente (son journal est porté par celui de la nouvelle).
        if adopt && let Some(mut f) = self.finishing.take() {
            f.take.merge = true;
            self.stalled_takes.push(f);
            self.notify("Disque lent · prise précédente en cours d'écriture", true);
        }
        // Prises précédentes restées dans l'ancien journal : intégrées maintenant que la
        // capture a son thread d'écriture (sauvegarde en arrière-plan juste après).
        if let Some(j) = late {
            let (n, cleanup) = recovery::apply_journal(&mut self.project, &j, &[]);
            if n > 0 {
                self.load_peaks();
                self.notify(format!("{n} prise(s) précédente(s) récupérée(s)"), false);
            }
            let dir = self.project.dir.clone();
            self.stash_stale_files(&dir, cleanup);
        }
        // 5) Session et « dernière session » (récupération au lancement) écrites en
        //    arrière-plan : jamais d'écriture forcée sur le disque avant la capture.
        self.save_session_background();
        let path = self.project.file_path();
        let _ = std::thread::Builder::new()
            .name("tracksplus-remember".into())
            .spawn(move || recovery::remember_session_path(&path));
        // Copie de sécurité : double de la session (fichier de session et journal compris)
        // sur le second disque, écrit dans un thread — un disque en veille ne retarde pas
        // la prise ; s'il est absent, la prise principale se fait quand même.
        if has_backup {
            self.backup_session_job(Some(journal.clone()), false);
            self.backup_double_at = self.now;
        }
        self.rec_has_backup = has_backup;
        if backup_blocked {
            self.notify("Copie de sécurité inactive : second disque bloqué", true);
        } else if self.backup_root.is_some() && !has_backup {
            self.notify("Copie de sécurité inactive : même disque", false);
        } else if self.backup_root.is_none()
            && let Some(e) = &self.backup_prefs_error
        {
            let msg = format!("Copie de sécurité inactive : {e}");
            self.notify(msg, true);
        }
        self.recorder = Some(recorder);
        self.disk_err_cache = None;
        self.rec_journal = Some(journal.clone());
        self.journal_pending = self.rec_overdub.then_some(journal);
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
        // Prise lancée (à la main ou par la relance automatique) : plus rien à relancer.
        self.resume = None;
        self.view.nav_until = 0.0;
        self.reveal_recording_tracks();
        // Une prise modifie la timeline : l'historique d'annulation d'avant ne s'applique
        // plus (sinon Ctrl+Z pourrait retirer la prise de la session).
        self.undo.clear();
        self.redo.clear();
        self.prune_peaks();
        // Surveillance du signal à partir de maintenant (et pas du début de l'image : une
        // préparation lente ne doit pas faire croire à une carte perdue).
        self.rec_watch = (0, self.now + t0.elapsed().as_secs_f64());
        self.gap_seen = 0;
        self.gap_count_seen = 0;
        self.stall_logged = false;
        self.incident(format!(
            "Prise lancée à {} ({} piste(s))",
            fmt_time(self.playhead, self.sr()),
            self.live.len()
        ));
        if !skipped.is_empty() {
            self.notify(
                format!(
                    "{} piste(s) sans entrée, NON enregistrée(s) : {}",
                    skipped.len(),
                    skipped.join(", ")
                ),
                true,
            );
        }
    }

    /// Départ de prise impossible après le début de la capture : tout est arrêté.
    fn abort_record_start(&mut self, msg: String) {
        self.engine
            .stop_record(std::time::Duration::from_millis(1500));
        self.engine.stop_playback();
        self.engine.shared.counting.store(false, Ordering::Relaxed);
        self.player = None;
        // Prise précédente toujours en fin d'écriture : sa forme d'onde reste affichée.
        self.live = self
            .finishing
            .as_ref()
            .map(|f| f.take.live.clone())
            .unwrap_or_default();
        self.rec_overdub = false;
        self.rec_latency = 0;
        self.notify(msg, true);
    }

    /// Sauvegarde de la session dans un thread, résultat suivi (voir `poll_rec_save`). Les
    /// fichiers inutiles de cette session en attente sont supprimés si elle réussit (elle
    /// part de l'état actuel : elle couvre tout ce qui a été intégré jusqu'ici).
    fn save_session_background(&mut self) {
        let dir = self.project.dir.clone();
        // Sauvegarde précédente encore en cours : la nouvelle, plus récente, la remplace.
        if let Some((d, _, old)) = self.rec_save.take() {
            self.stash_stale_files(&d, old);
        }
        let cleanup = self.take_stale_files(&dir);
        let rx = self.project.save_in_background();
        self.rec_save = Some((dir, rx, cleanup));
    }

    /// Fichiers inutiles de la session `dir` en attente de suppression (retirés de la liste).
    fn take_stale_files(&mut self, dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        self.stale_files.retain(|(d, f)| {
            if d == dir {
                out.push(f.clone());
                false
            } else {
                true
            }
        });
        out
    }

    /// Fichiers à supprimer après la prochaine sauvegarde réussie de la session `dir`.
    fn stash_stale_files(&mut self, dir: &Path, files: Vec<PathBuf>) {
        self.stale_files
            .extend(files.into_iter().map(|f| (dir.to_path_buf(), f)));
        if self.stale_files.len() > MAX_STALE_FILES {
            let extra = self.stale_files.len() - MAX_STALE_FILES;
            self.stale_files.drain(..extra);
        }
    }

    fn poll_rec_save(&mut self) {
        let Some((dir, rx, _)) = &self.rec_save else {
            return;
        };
        let same = *dir == self.project.dir;
        match rx.try_recv() {
            Ok(Ok(())) => {
                // Session sauvegardée avec les prises : leurs fichiers inutiles peuvent partir.
                if let Some((_, _, cleanup)) = self.rec_save.take() {
                    for f in cleanup {
                        let _ = std::fs::remove_file(f);
                    }
                }
            }
            Ok(Err(e)) => {
                if let Some((d, _, cleanup)) = self.rec_save.take() {
                    self.stash_stale_files(&d, cleanup);
                }
                if same {
                    // Nouvel essai par la sauvegarde automatique (active aussi pendant une prise).
                    self.project.dirty = true;
                    self.notify(
                        format!("Session non sauvegardée · nouvel essai : {e:#}"),
                        true,
                    );
                }
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                if let Some((d, _, cleanup)) = self.rec_save.take() {
                    self.stash_stale_files(&d, cleanup);
                }
                if same {
                    self.project.dirty = true;
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
    }

    /// Journaux des prises de cette session encore en cours d'écriture (prises restées
    /// bloquées, fin de prise en cours), sans les prises en attente qu'ils portent.
    fn inflight_journals(&self) -> Vec<recovery::Journal> {
        let mut v = journals_of(
            self.stalled_takes
                .iter()
                .chain(self.finishing.as_ref())
                .map(|f| &f.take),
            &self.project.dir,
        );
        // Prises abandonnées à la fermeture (disque qui ne répond plus) : toujours à
        // récupérer à la prochaine ouverture, jamais retirées du journal.
        v.extend(
            self.given_up
                .iter()
                .filter(|(d, _)| *d == self.project.dir)
                .map(|(_, j)| j.clone()),
        );
        v
    }

    /// Prise intégrée et session sauvegardée (aucune prise en cours ni en fin d'écriture
    /// normale) : le journal ne garde que les prises encore bloquées, ou disparaît.
    fn settle_journal(&mut self) {
        match remaining_journal(self.inflight_journals(), self.project.sample_rate) {
            None => recovery::remove_journal(&self.project.dir),
            Some(j) => {
                if let Err(e) = recovery::write_journal(&self.project.dir, &j) {
                    self.notify(format!("Journal de secours non écrit : {e:#}"), true);
                }
            }
        }
    }

    /// Session sauvegardée après l'intégration tardive d'une prise (prise restée bloquée,
    /// ou récupérée depuis ses fichiers) ; `cleanup` (positions notées, fichiers vides)
    /// supprimé seulement une fois la sauvegarde réussie. Pendant une autre prise ou une
    /// autre fin de prise : en arrière-plan, journal laissé tel quel (c'est leur thread
    /// d'écriture qui le tient) ; sinon tout de suite, et journal réduit aux prises encore
    /// en cours. Rend l'erreur d'une sauvegarde immédiate.
    fn save_after_late_take(&mut self, cleanup: Vec<PathBuf>) -> Option<anyhow::Error> {
        self.project.dirty = true;
        let dir = self.project.dir.clone();
        self.stash_stale_files(&dir, cleanup);
        if self.is_recording() || self.finishing.is_some() {
            self.save_session_background();
            return None;
        }
        match self.project.save() {
            Ok(()) => {
                for f in self.take_stale_files(&dir) {
                    let _ = std::fs::remove_file(f);
                }
                self.settle_journal();
                None
            }
            Err(e) => Some(e),
        }
    }

    /// Fin de prise sans aucun progrès de l'écriture depuis `FINISH_STALLED_SECS`.
    fn finish_stalled(&self) -> bool {
        self.finishing
            .as_ref()
            .is_some_and(|f| self.now - f.seen.1 > FINISH_STALLED_SECS)
    }

    /// Formes d'onde des prises qui n'existent plus nulle part (ni dans la session, ni dans
    /// l'historique d'annulation) : mémoire rendue (journée de balances, prises refaites).
    fn prune_peaks(&mut self) {
        let mut used: HashSet<u64> = HashSet::new();
        let tracks = self.project.tracks.iter().chain(
            self.undo
                .iter()
                .chain(&self.redo)
                .flat_map(|s| s.tracks.iter()),
        );
        for t in tracks {
            used.extend(t.clips.iter().map(|c| c.id));
        }
        self.peaks.retain(|id, _| used.contains(id));
    }

    pub fn stop(&mut self) {
        match self.transport {
            // Stop à l'arrêt : l'opérateur reprend la main, plus de relance automatique.
            Transport::Stopped => self.cancel_resume(),

            Transport::Playing { .. } => {
                self.engine.stop_playback();
                self.player = None;
            }
            Transport::Recording { start } => {
                let len = self.engine.shared.rec_frames.load(Ordering::Relaxed);
                self.incident(format!(
                    "Prise arrêtée ({} enregistrées depuis {})",
                    fmt_time(len, self.sr()),
                    fmt_time(start, self.sr())
                ));
                // 1) Le callback rend les cibles ; leurs producteurs sont libérés ici.
                let released = self
                    .engine
                    .stop_record(std::time::Duration::from_millis(1500));
                self.engine.stop_playback();
                self.player = None;
                // 2) Le thread disque écrit tout ce qui reste puis ferme les fichiers, dans
                //    un thread : l'interface ne se fige pas, même si le disque est lent.
                self.awake = None;
                let sh = &self.engine.shared;
                let mut take = TakeEnd {
                    released,
                    live: self.live.clone(),
                    lat: std::mem::take(&mut self.rec_latency),
                    lost_card: std::mem::take(&mut self.lost_during_take),
                    split: std::mem::take(&mut self.take_split),
                    gap: sh.rec_gap_frames.swap(0, Ordering::Relaxed),
                    gap_missing: sh.rec_gap_overflow.load(Ordering::Relaxed),
                    backup_warn: Vec::new(),
                    overruns: sh.overruns.load(Ordering::Relaxed),
                    xruns: sh.xruns.load(Ordering::Relaxed),
                    merge: false,
                    dir: self.project.dir.clone(),
                    // Sans les prises bloquées qu'il porte : elles ont leur propre attente.
                    journal: self.rec_journal.take().map(|mut j| {
                        j.extra.clear();
                        j
                    }),
                };
                self.journal_pending = None;
                match self.recorder.take() {
                    Some(rec) => {
                        take.backup_warn = rec
                            .errors()
                            .into_iter()
                            .filter(|e| e.starts_with("copie"))
                            .collect();
                        if self.engine.shared.copy_overruns.load(Ordering::Relaxed) > 0 {
                            take.backup_warn
                                .push("second disque trop lent (silence)".into());
                        }
                        let progress = rec.progress();
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
                        let p = progress.load(Ordering::Relaxed);
                        self.finishing = Some(Finishing {
                            rx,
                            start: start + offset,
                            take,
                            progress,
                            seen: (p, self.now),
                            slow_told: false,
                        });
                    }
                    None => self.live_takes_to_clips(start, TakeOutcome::Lost, take),
                }
            }
        }
        self.transport = Transport::Stopped;
        self.engine.shared.counting.store(false, Ordering::Relaxed);
    }

    /// Prise faite en écoutant : dès qu'elle a commencé, le journal de secours reçoit le
    /// décalage mesuré et la latence, pour qu'une prise récupérée après un crash tombe en
    /// place elle aussi. Écrit par le thread d'écriture (après le journal initial).
    fn complete_journal(&mut self) {
        if !self.is_recording() || self.engine.shared.rec_frames.load(Ordering::Acquire) == 0 {
            return;
        }
        let Some(mut j) = self.journal_pending.take() else {
            return;
        };
        j.play_offset = self.engine.shared.rec_play_offset.load(Ordering::Relaxed);
        j.latency = self.rec_latency;
        if let Some(r) = &self.recorder {
            r.update_journal(self.project.dir.clone(), j.clone());
        }
        self.rec_journal = Some(j.clone());
        // Journal complété aussi sur la copie de sécurité (prise recalée correctement si
        // c'est elle qu'on récupère).
        if self.rec_has_backup {
            self.backup_session_job(Some(j), false);
        }
    }

    /// Attente de la fin d'écriture d'une prise à la fermeture de l'application : aussi
    /// longtemps qu'elle avance, abandon après `idle` sans aucun progrès ou `cap` en tout.
    fn wait_finish_for(
        f: &Finishing,
        idle: std::time::Duration,
        cap: std::time::Duration,
    ) -> TakeOutcome {
        use std::sync::mpsc::RecvTimeoutError;
        let t0 = std::time::Instant::now();
        let mut seen = (
            f.progress.load(Ordering::Relaxed),
            std::time::Instant::now(),
        );
        loop {
            match f
                .rx
                .recv_timeout(idle.min(std::time::Duration::from_millis(100)))
            {
                Ok(r) => return TakeOutcome::Done(r),
                Err(RecvTimeoutError::Disconnected) => return TakeOutcome::Lost,
                Err(RecvTimeoutError::Timeout) => {
                    let p = f.progress.load(Ordering::Relaxed);
                    if p != seen.0 {
                        seen = (p, std::time::Instant::now());
                    }
                    if seen.1.elapsed() > idle || t0.elapsed() > cap {
                        return TakeOutcome::StillWriting;
                    }
                }
            }
        }
    }

    /// Fin de prise terminée en arrière-plan : on intègre les prises. `wait` : attendre
    /// (fermeture de l'application), au plus (sans progrès, en tout) ; le résultat n'est
    /// jamais jeté, même très tardif.
    fn poll_finishing(&mut self, wait: Option<(std::time::Duration, std::time::Duration)>) {
        render::set_recording(self.take_in_progress());
        self.poll_rec_save();
        self.poll_backups();
        // Prise la plus récente d'abord (son journal porte celles restées bloquées).
        self.poll_last_finishing(wait);
        // Prises restées bloquées : complétées dès que leur disque a fini.
        let mut i = 0;
        while i < self.stalled_takes.len() {
            let f = &self.stalled_takes[i];
            let out = if let Some((idle, cap)) = wait {
                Self::wait_finish_for(f, idle, cap)
            } else {
                match f.rx.try_recv() {
                    Ok(r) => TakeOutcome::Done(r),
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        i += 1;
                        continue;
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => TakeOutcome::Lost,
                }
            };
            let f = self.stalled_takes.remove(i);
            self.live_takes_to_clips(f.start, out, f.take);
        }
    }

    fn poll_last_finishing(&mut self, wait: Option<(std::time::Duration, std::time::Duration)>) {
        let Some(f) = &mut self.finishing else {
            return;
        };
        let out = if let Some((idle, cap)) = wait {
            Self::wait_finish_for(f, idle, cap)
        } else {
            match f.rx.try_recv() {
                Ok(r) => TakeOutcome::Done(r),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => TakeOutcome::Lost,
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    let p = f.progress.load(Ordering::Relaxed);
                    if p != f.seen.0 {
                        f.seen = (p, self.now);
                    }
                    let idle = self.now - f.seen.1;
                    if idle > FINISH_SLOW_SECS && !f.slow_told {
                        f.slow_told = true;
                        self.notify("Disque lent · fin de prise en cours d'écriture", true);
                    }
                    return;
                }
            }
        };
        let f = self.finishing.take().expect("prise en finalisation");
        self.live_takes_to_clips(f.start, out, f.take);
    }

    /// Ajoute les prises terminées à la session, la sauvegarde et nettoie le journal.
    fn live_takes_to_clips(&mut self, start: u64, outcome: TakeOutcome, take: TakeEnd) {
        let TakeEnd {
            released,
            live,
            lat,
            lost_card,
            split,
            gap,
            gap_missing,
            backup_warn,
            overruns,
            xruns,
            merge,
            dir,
            journal,
        } = take;
        if !merge {
            self.live.clear();
        }
        if dir != self.project.dir {
            // Une autre session a été ouverte entre-temps : la prise sera intégrée à la
            // prochaine ouverture de la sienne (journal de secours, fichiers terminés).
            if let Some(j) = &journal {
                let _ = recovery::keep_journal(&dir, j);
            }
            self.notify(
                format!("Prise précédente terminée · gardée pour {}", dir.display()),
                false,
            );
            return;
        }
        let (results, backup) = match outcome {
            TakeOutcome::Done(Finished {
                main: Ok(r),
                backup,
            }) => (r, backup),
            TakeOutcome::StillWriting => {
                // Fermeture de l'application, disque bloqué : le journal est conservé ; les
                // prises seront réparées et réintégrées à la prochaine ouverture. Elle reste
                // comptée parmi les prises en cours : une autre prise intégrée ensuite ne
                // la retire jamais du journal (`inflight_journals`).
                if let Some(j) = &journal {
                    if !self.is_recording() {
                        let _ = recovery::keep_journal(&dir, j);
                    }
                    self.given_up.push((dir.clone(), j.clone()));
                }
                self.take_notes(split, gap, gap_missing, lost_card, None);
                self.notify("Disque bloqué · prises récupérées à la réouverture", true);
                return;
            }
            TakeOutcome::Done(Finished { main: Err(_), .. }) | TakeOutcome::Lost => {
                // Thread disque en échec : on récupère la prise depuis ses fichiers grâce à
                // son journal (jamais celles d'autres prises encore en cours d'écriture).
                if !merge {
                    self.engine.shared.overruns.store(0, Ordering::Relaxed);
                }
                let Some(j) = &journal else {
                    // Pas de journal en mémoire : celui de la session, sans les prises
                    // encore en cours d'écriture.
                    let live = self.inflight_journals();
                    self.take_notes(split, gap, gap_missing, lost_card, None);
                    match recovery::recover_with(&mut self.project, &live) {
                        Ok(r) => {
                            self.load_peaks();
                            let msg = match &r.saved {
                                Ok(()) => {
                                    format!("Erreur disque : {} prise(s) récupérée(s)", r.takes)
                                }
                                Err(e) => format!(
                                    "Erreur disque : {} prise(s) récupérée(s) · session non sauvegardée : {e:#}",
                                    r.takes
                                ),
                            };
                            self.notify(msg, true);
                        }
                        Err(e) => self.notify(
                            format!("Erreur disque · journal de secours illisible : {e:#}"),
                            true,
                        ),
                    }
                    return;
                };
                self.take_notes(split, gap, gap_missing, lost_card, None);
                let (n, cleanup) = recovery::apply_journal(&mut self.project, j, &[]);
                // La session a changé : l'historique d'avant ne s'applique plus (Ctrl+Z ne
                // doit pas retirer la prise récupérée).
                self.undo.clear();
                self.redo.clear();
                self.load_peaks();
                self.prune_peaks();
                let msg = match self.save_after_late_take(cleanup) {
                    None => format!("Erreur disque : {n} prise(s) récupérée(s)"),
                    Some(e) => {
                        // Journal de la prise gardé : la récupération se refera.
                        let _ = recovery::keep_journal(&dir, j);
                        format!(
                            "Erreur disque : {n} prise(s) récupérée(s) · session non sauvegardée : {e:#}"
                        )
                    }
                };
                self.notify(msg, true);
                return;
            }
        };
        let mut errors = Vec::new();
        let mut max_len = 0;
        for r in &results {
            if let Some(e) = &r.error {
                errors.push(e.clone());
            }
            max_len = max_len.max(r.frames());
        }
        // Une prise longue peut être répartie sur plusieurs fichiers : une prise par
        // fichier, chacune à sa position dans la prise (bout à bout, ou avec un trou
        // après une erreur d'écriture), recalée de la latence.
        let layout = {
            let project = &mut self.project;
            layout_take(&results, start, lat, &mut || project.alloc_id())
        };
        let audio = self.project.audio_dir();
        let mut main_clips = Vec::new();
        for (track_id, clip, seg) in layout.clips {
            let path = audio.join(&seg.file);
            // Déjà dans la session (réintégré depuis le disque à la réouverture de la session
            // ou après un crash) : longueur réelle, jamais de doublon.
            let ids = extend_existing(&mut self.project, &clip.file, seg.frames);
            if !ids.is_empty() {
                let pk: Peaks = Arc::new(RwLock::new(PeakData::default()));
                render::request_peaks(path, pk.clone());
                for id in ids {
                    self.peaks.insert(id, pk.clone());
                }
                continue;
            }
            if let Some(p) = live
                .iter()
                .find(|l| l.track_id == track_id)
                .map(|l| &l.peaks)
            {
                let take_len = results
                    .iter()
                    .find(|r| r.track_id == track_id)
                    .map_or(0, |r| r.frames());
                let whole = results
                    .iter()
                    .find(|r| r.track_id == track_id)
                    .is_some_and(|r| r.segments.len() == 1)
                    && seg.start == 0
                    && seg.frames == take_len;
                let block = render::PEAK_BLOCK as u64;
                if whole {
                    // La forme d'onde en direct couvre ce fichier : gardée telle quelle, mise
                    // en cache en arrière-plan (pas de gel au Stop d'une longue prise).
                    render::store_peaks_later(path, p.clone());
                    self.peaks.insert(clip.id, p.clone());
                } else if seg.start % block == 0 {
                    // Fichier d'une prise longue, commençant sur un point de la forme
                    // d'onde en direct : sa part en est découpée, sans relire l'audio.
                    let pk: Peaks = Arc::new(RwLock::new(PeakData::default()));
                    let a = (seg.start / block) as usize;
                    let b = (seg.start + seg.frames).div_ceil(block) as usize;
                    render::slice_peaks_later(p.clone(), a, b, path, pk.clone());
                    self.peaks.insert(clip.id, pk);
                } else {
                    // Fichier repris après une erreur, à une position quelconque : calculé
                    // depuis le fichier.
                    let pk: Peaks = Arc::new(RwLock::new(PeakData::default()));
                    render::request_peaks(path, pk.clone());
                    self.peaks.insert(clip.id, pk);
                }
            }
            if let Some(tr) = self.project.track_mut(track_id) {
                tr.clips.push(clip.clone());
                main_clips.push((track_id, clip));
            }
        }
        // Positions notées et fichiers vides : inutiles une fois la session sauvegardée avec
        // ces prises (gardés jusque-là : la récupération après un crash en a besoin).
        let cleanup: Vec<PathBuf> = layout
            .discard
            .iter()
            .map(|f| audio.join(f))
            .chain(results.iter().flat_map(|r| {
                r.segments
                    .iter()
                    .map(|seg| audio.join(crate::recorder::start_sidecar(&seg.file)))
            }))
            .collect();
        // Une prise modifie la timeline : l'historique d'annulation d'avant ne s'applique
        // plus (sinon Ctrl+Z pourrait retirer la prise de la session).
        self.undo.clear();
        self.redo.clear();
        self.prune_peaks();
        if merge {
            // Une autre prise peut être en cours : sauvegarde en arrière-plan, journal laissé
            // en place (tenu par son thread d'écriture). Sinon (arrêt, fermeture), sauvegarde
            // tout de suite et journal à jour.
            if let Some(e) = self.save_after_late_take(cleanup) {
                self.save_failed(e);
            }
            if let Some(b) = backup {
                self.backup_pending.push(PendingBackup {
                    rx: b.rx,
                    start,
                    lat,
                    dir: self.project.dir.clone(),
                    main: main_clips,
                });
            }
            self.notify(
                format!(
                    "Prise précédente complétée · {}",
                    fmt_time(max_len, self.sr())
                ),
                !errors.is_empty(),
            );
            self.take_notes(split, gap, gap_missing, lost_card, Some(max_len));
            return;
        }
        self.playhead = start + max_len;
        self.project.dirty = true;
        // L'état d'avant la prise rejoint les versions précédentes (`.bak`).
        let save = self.project.save_checkpoint();
        let dir = self.project.dir.clone();
        if save.is_ok() {
            // Les prises sont référencées dans la session : le journal, les positions
            // notées et les fichiers vides ne servent plus.
            for f in cleanup.into_iter().chain(self.take_stale_files(&dir)) {
                let _ = std::fs::remove_file(f);
            }
            self.settle_journal();
        } else {
            self.stash_stale_files(&dir, cleanup);
        }
        // Copie de sécurité : son résultat arrive à son rythme ; le double de la session
        // ne référence ses fichiers qu'une fois connus (voir `poll_backups`).
        if let Some(b) = backup {
            self.backup_pending.push(PendingBackup {
                rx: b.rx,
                start,
                lat,
                dir: self.project.dir.clone(),
                main: main_clips,
            });
        }
        if save.is_ok() {
            let done = self.backup_pending.is_empty();
            self.backup_session_job(None, done);
        }
        self.engine.shared.overruns.store(0, Ordering::Relaxed);
        if !errors.is_empty() {
            let mut msg = format!("Erreur d'enregistrement : {}", join_few(&errors, 3));
            if let Err(e) = &save {
                msg += &format!(" · Session non sauvegardée : {e:#}");
            }
            self.notify(msg, true);
        } else if let Err(e) = save {
            self.notify(
                format!("Prise sur le disque · session non sauvegardée : {e:#}"),
                true,
            );
        } else if overruns > 0 {
            self.notify(
                format!("Prise enregistrée · {overruns} saturations disque (silence)"),
                true,
            );
        } else if xruns > 0 {
            self.notify(
                format!(
                    "Prise enregistrée · {} · {xruns} décrochage{} · augmentez le buffer",
                    fmt_time(max_len, self.sr()),
                    if xruns > 1 { "s" } else { "" }
                ),
                true,
            );
        } else if !backup_warn.is_empty() {
            self.notify(
                format!(
                    "Prise enregistrée · copie de sécurité incomplète : {}",
                    join_few(&backup_warn, 3)
                ),
                true,
            );
        } else if !released {
            self.notify("Prise enregistrée · carte son muette à l'arrêt", true);
        } else {
            self.notify(
                format!(
                    "Prise enregistrée et sauvegardée · {}",
                    fmt_time(max_len, self.sr())
                ),
                false,
            );
        }
        self.take_notes(split, gap, gap_missing, lost_card, Some(max_len));
    }

    /// Ce qu'il faut retenir d'une prise, quelle que soit la façon dont elle a fini :
    /// entrée interrompue puis revenue (trou comblé, ou plus long que les tampons : prise
    /// coupée, la suite relancée à sa place), carte son perdue. Ces messages s'ajoutent aux
    /// précédents, ils ne les effacent pas.
    fn take_notes(
        &mut self,
        split: bool,
        gap: u64,
        gap_missing: u64,
        lost_card: bool,
        len: Option<u64>,
    ) {
        let sr = self.sr();
        if split {
            let next = if self.resume.is_some() {
                "suite relancée"
            } else {
                "relancez la prise"
            };
            self.notify_level(
                format!(
                    "Entrée coupée {} · prise sauvegardée · {next}",
                    fmt_gap(gap_missing as f64 / sr as f64)
                ),
                Level::Critical,
            );
        } else if gap > 0 {
            self.notify(
                format!(
                    "Entrée coupée {} · silence inséré",
                    fmt_gap(gap as f64 / sr as f64)
                ),
                true,
            );
        }
        if lost_card {
            let next = if self.resume.is_some() {
                "relance à son retour"
            } else {
                "relancez la prise"
            };
            let kept = match len {
                Some(n) => format!("prise sauvegardée ({})", fmt_time(n, sr)),
                None => "prise gardée".to_string(),
            };
            self.notify_level(
                format!("Carte son perdue · {kept} · {next}"),
                Level::Critical,
            );
        }
    }

    /// Résultats des copies de sécurité arrivés : le double de la session sur le second
    /// disque référence alors les fichiers réellement présents sur ce disque.
    fn poll_backups(&mut self) {
        let mut i = 0;
        while i < self.backup_pending.len() {
            match self.backup_pending[i].rx.try_recv() {
                Ok(res) => {
                    let p = self.backup_pending.remove(i);
                    self.integrate_backup(p, res);
                }
                Err(crossbeam_channel::TryRecvError::Empty) => i += 1,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    self.backup_pending.remove(i);
                }
            }
        }
    }

    fn integrate_backup(&mut self, p: PendingBackup, res: Result<Vec<RecResult>, FinishError>) {
        // Session changée entre-temps, ou copie interrompue : le journal de la copie reste
        // sur le second disque (la récupération de la copie posera ses fichiers).
        let Ok(results) = res else {
            return;
        };
        if p.dir != self.project.dir {
            return;
        }
        let layout = {
            let project = &mut self.project;
            layout_take(&results, p.start, p.lat, &mut || project.alloc_id())
        };
        let backup: Vec<(u64, Clip)> = layout.clips.into_iter().map(|(t, c, _)| (t, c)).collect();
        for (track_id, main, backup) in backup_divergence(&p.main, &backup) {
            self.backup_alt.push(BackupAlt {
                dir: p.dir.clone(),
                track_id,
                main,
                backup,
            });
        }
        let done = !self.is_recording()
            && self.finishing.is_none()
            && self.stalled_takes.is_empty()
            && self.backup_pending.is_empty();
        self.backup_session_job(None, done);
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
            self.notify("Appuyez encore pour arrêter", true);
        }
    }

    /// Carte son disparue (DVS relancé, câble USB, pilote réinitialisé, horloge Dante
    /// perdue) : comme les logiciels pro, chaque flux est surveillé en permanence (même
    /// fenêtre réduite) et seul celui qui est mort est rouvert, tout seul, sans toucher à
    /// ceux qui marchent. Une prise arrêtée par la perte de l'entrée est relancée dès son
    /// retour, à la bonne position (réglage « Relancer la prise »).
    fn watch_device(&mut self) {
        self.poll_reconnect();
        self.watch_streams();
        self.watch_take();
        self.resume_take();
        self.schedule_reconnect();
    }

    /// Battement de cœur de chaque flux ouvert : un flux qui ne tourne plus, même sans
    /// erreur du pilote (horloge Dante perdue, pilote figé), est signalé perdu. S'il repart
    /// de lui-même avant d'avoir été rouvert (ASIO pendant une prise), la perte s'efface :
    /// le pilote n'est pas redémarré pour rien à l'arrêt de la prise.
    fn watch_streams(&mut self) {
        let now = self.now;
        // Saut dans le temps (veille, interface figée) : rien n'est jugé sur cet intervalle.
        let jumped = now - self.hb_tick > 1.5;
        self.hb_tick = now;
        let open = self.engine.open_mask();
        let rec = self.is_recording();
        let sh = &self.engine.shared;
        let beats = std::array::from_fn(|s| sh.heartbeat[s].load(Ordering::Relaxed));
        let (stalled, beating) = watchdog(&mut self.hb, beats, open, now, jumped, rec);
        // Flux ouverts depuis le dernier passage : pas encore démarrés.
        self.started &= !(open & !self.open_seen);
        self.open_seen = open;
        self.started = (self.started | beating) & open;
        // Jamais démarré depuis son ouverture : le pilote ne tourne pas encore (Dante
        // Virtual Soundcard sans horloge Dante sur le réseau). Rien n'est perdu ni rouvert
        // (rouvrir n'y changerait rien) : il démarrera de lui-même, on le dit une fois.
        let never = stalled & !self.started;
        let stalled = stalled & self.started;
        let waiting = never | (self.not_started & open & !self.started);
        if waiting != 0 && self.not_started == 0 {
            self.not_started_since = now;
            if !self.clockless {
                let msg = self.not_started_message(waiting);
                self.device_notice(msg);
            }
        } else if waiting == 0 && (self.not_started != 0 || self.clockless) && beating & open != 0 {
            self.clockless = false;
            self.device_note.clear();
            self.notify("Carte son prête", false);
        }
        self.not_started = waiting;
        // ASIO : un pilote qui ne démarre pas n'est pas gardé (l'application du pilote,
        // Dante Virtual Soundcard par exemple, resterait verrouillée, ses réglages grisés).
        // Relâché, puis réessayé de temps en temps (essais de plus en plus espacés).
        let release = waiting != 0
            && self.prefs.host.as_deref() == Some("ASIO")
            && now - self.not_started_since > NOT_STARTED_RELEASE
            && !self.take_in_progress();
        let sh = &self.engine.shared;
        // (Un flux fermé n'est plus « seulement muet » : il est rouvert ou absent.)
        self.stalled &= open;
        let before = sh.device_lost.fetch_or(stalled, Ordering::AcqRel);
        self.stalled |= stalled & !before;
        let revived = beating & self.stalled & !sh.stream_dead.load(Ordering::Acquire);
        if revived != 0 {
            sh.device_lost.fetch_and(!revived, Ordering::AcqRel);
            self.stalled &= !revived;
            self.announce = true;
        }
        self.awaiting &= !beating;
        if release {
            self.clockless = true;
            self.not_started = 0;
            self.engine.pad(PadCmd::StopAll);
            self.engine.release();
            self.hb = [(0, now); audio::SLOTS];
            self.open_seen = 0;
            self.started = 0;
        }
    }

    /// Message pour des flux ouverts qui n'ont jamais démarré (`watch_streams`).
    fn not_started_message(&self, slots: u8) -> String {
        let st = &self.engine.status;
        let dante = [&st.input, &st.output, &st.pad_output, &st.monitor_output]
            .into_iter()
            .flatten()
            .any(|n| n.to_lowercase().contains("dante"));
        if dante {
            "Dante : pas d'horloge réseau".to_string()
        } else {
            format!("Carte son muette : {}", self.engine.slots_label(slots))
        }
    }

    /// Pendant une prise. Entrée muette sans erreur du pilote (horloge Dante perdue
    /// quelques secondes) : la prise continue (alerte rouge, trou comblé par du silence à
    /// la reprise). Trou plus long que les tampons : la prise est coupée au bout de ce qui a
    /// pu être comblé, et la suite relancée là où tombe le son qui revient. Entrée vraiment
    /// perdue (erreur du pilote, ou muette plus de 20 s) : prise arrêtée et sauvegardée
    /// telle quelle, relancée dès le retour de la carte.
    fn watch_take(&mut self) {
        if !self.is_recording() {
            self.rec_watch = (0, self.now);
            return;
        }
        let sh = &self.engine.shared;
        let frames = sh.rec_frames.load(Ordering::Relaxed);
        // Suivi ici aussi (et pas seulement dans l'affichage) : fonctionne fenêtre réduite.
        if frames != self.rec_watch.0 {
            self.rec_watch = (frames, self.now);
        }
        let Transport::Recording { start } = self.transport else {
            return;
        };
        let offset = if self.rec_overdub {
            sh.rec_play_offset.load(Ordering::Relaxed)
        } else {
            0
        };
        let base = start + offset + frames;
        let sr = self.sr();
        // Silence inséré à la place d'un trou de l'entrée : repère à cet endroit.
        // (Acquire : la position du trou, écrite avant, est lue à jour.)
        let gap = sh.rec_gap_frames.load(Ordering::Acquire);
        if gap > self.gap_seen {
            let d = gap - self.gap_seen;
            let at = start + offset + sh.rec_gap_at.load(Ordering::Relaxed);
            let count = sh.rec_gap_count.load(Ordering::Relaxed);
            let several = count.saturating_sub(self.gap_count_seen);
            self.gap_seen = gap;
            self.gap_count_seen = count;
            let len = fmt_gap(d as f64 / sr as f64);
            if self.stall_logged {
                self.stall_logged = false;
                self.incident(format!("Signal d'entrée revenu après un trou de {len}"));
            }
            let name = if several > 1 {
                format!("{several} trous, {len} en tout (silence)")
            } else {
                format!("Trou {len} (silence)")
            };
            self.incident_marker(at, name);
        }
        let sh = &self.engine.shared;
        // Signal d'entrée interrompu (pas encore de trou comblé) : noté une fois.
        if !self.stall_logged && self.now - self.rec_watch.1 > 1.0 {
            self.stall_logged = true;
            self.incident(format!(
                "Plus de signal d'entrée pendant la prise, à {} : la prise reste ouverte",
                fmt_time(base, sr)
            ));
        } else if self.stall_logged && self.now - self.rec_watch.1 < 0.5 {
            self.stall_logged = false;
            self.incident("Signal d'entrée revenu");
        }
        let missing = sh.rec_gap_overflow.load(Ordering::Acquire);
        if missing > 0 {
            // Le son qui revient tombe `missing` trames après la fin de la prise (figée),
            // capté il y a `ago` secondes.
            let ago = sh.secs_since(sh.rec_gap_overflow_ns.load(Ordering::Relaxed));
            self.resume = self.prefs.auto_restart.then(|| Resume {
                pos: base + missing,
                at: self.now - ago,
                dir: self.project.dir.clone(),
                hb_seen: None,
            });
            self.take_split = true;
            self.incident_marker(
                base,
                format!("Coupure : trou {}", fmt_gap(missing as f64 / sr as f64)),
            );
            self.stop();
            return;
        }
        let lost = sh.device_lost.load(Ordering::Acquire) & audio::LOST_INPUT != 0;
        if !lost && self.now - self.rec_watch.1 <= REC_STALL_STOP {
            return;
        }
        sh.device_lost
            .fetch_or(audio::LOST_INPUT, Ordering::Release);
        self.resume = self.prefs.auto_restart.then(|| Resume {
            pos: base,
            at: self.rec_watch.1,
            dir: self.project.dir.clone(),
            hb_seen: None,
        });
        self.lost_during_take = true;
        self.incident_marker(base, "Carte son perdue".into());
        self.stop();
    }

    /// Relance automatique en attente : annulée (l'opérateur reprend la main : Stop,
    /// Échap, panique, indicateur du transport).
    pub fn cancel_resume(&mut self) {
        if self.resume.take().is_some() {
            self.notify_level("Relance auto annulée", Level::Critical);
        }
    }

    /// Relance de la prise arrêtée par une perte de la carte, dès que l'entrée délivre de
    /// nouveau des trames : nouvelles prises sur les mêmes pistes armées, posées à l'heure
    /// réelle (fin de ce qui a été capté + durée du trou). Jamais au-delà de `RESUME_MAX`.
    fn resume_take(&mut self) {
        let Some(r) = &self.resume else {
            return;
        };
        let sh = &self.engine.shared;
        let lost = sh.device_lost.load(Ordering::Acquire);
        let step = resume_step(&ResumeCtx {
            age: self.now - r.at,
            same_session: r.dir == self.project.dir,
            enabled: self.prefs.auto_restart,
            // Disque de la prise précédente bloqué depuis plusieurs prises : `record` la
            // refuserait. Bloqué depuis peu : la nouvelle prise part quand même (la
            // précédente rejoint les prises en attente, voir `over_finishing`).
            disk_stalled: matches!(
                over_finishing(
                    self.finishing.is_some(),
                    self.finish_stalled(),
                    self.stalled_takes.len(),
                ),
                OverFinishing::TooMany
            ),
            transport_stopped: matches!(self.transport, Transport::Stopped),
            idle: matches!(self.transport, Transport::Stopped)
                && (self.finishing.is_none() || self.finish_stalled()),
            input_ready: lost & audio::LOST_INPUT == 0
                && self.engine.reopening() & audio::LOST_INPUT == 0
                && self.engine.open_mask() & audio::LOST_INPUT != 0,
            beat: sh.heartbeat[audio::SLOT_INPUT].load(Ordering::Relaxed),
            hb_seen: r.hb_seen,
        });
        let (pos, at) = (r.pos, r.at);
        match step {
            ResumeStep::Wait => {}
            ResumeStep::Arm(beat) => {
                if let Some(r) = &mut self.resume {
                    r.hb_seen = Some(beat);
                }
            }
            // Autre session, réglage désactivé, lecture lancée entre-temps : l'opérateur a
            // repris la main, rien n'est relancé derrière son dos.
            ResumeStep::Cancel => self.resume = None,
            ResumeStep::Blocked => {
                self.resume = None;
                self.notify_level("Relance impossible : disque bloqué", Level::Critical);
            }
            ResumeStep::Expire => {
                self.resume = None;
                self.notify_level(
                    format!(
                        "Relance abandonnée : carte absente depuis {} min",
                        (RESUME_MAX / 60.0).round()
                    ),
                    Level::Critical,
                );
            }
            ResumeStep::Go => {
                let gap = (self.now - at).max(0.0);
                self.resume = None;
                self.playhead = resume_position(pos, gap, self.sr());
                self.record();
                if self.is_recording() {
                    self.incident_marker(
                        self.playhead,
                        format!("Reprise auto (trou {})", fmt_gap(gap)),
                    );
                    self.notify_level(
                        format!("Prise relancée · trou de {}", fmt_gap(gap)),
                        Level::Critical,
                    );
                } else {
                    self.notify_level("Relance impossible · relancez la prise", Level::Critical);
                }
            }
        }
    }

    /// Flux retirés par les essais en cours ou bloqués.
    fn jobs_slots(&self) -> u8 {
        self.stuck_jobs
            .iter()
            .chain(&self.reconnect_job)
            .fold(0, |m, j| m | j.slots)
    }

    /// Flux perdus ou absents : essais de réouverture espacés (1, 2, 4… jusqu'à 10 s), un
    /// seul à la fois, dans un thread (pas de gel de l'écran) ; seuls les flux concernés
    /// sont rouverts.
    fn schedule_reconnect(&mut self) {
        use crate::audio::LOST_OUTPUT;
        let lost = self.engine.shared.device_lost.load(Ordering::Acquire);
        // Flux d'un essai bloqué chez le pilote : attendus à part, pas rouverts en double.
        let stuck = self.stuck_jobs.iter().fold(0, |m, j| m | j.slots);
        let need = (lost | self.engine.missing()) & !stuck;
        self.awaiting &= self.engine.open_mask() & !lost;
        if need == 0 {
            if self.reconnect_job.is_none() && stuck == 0 && self.awaiting == 0 {
                // Tout tourne vraiment (premier callback reçu) : c'est maintenant seulement
                // que la carte est dite reconnectée.
                if std::mem::take(&mut self.announce) {
                    self.notify("Carte son reconnectée", false);
                }
                self.reconnect = None;
                self.reconnect_wait = false;
                self.device_note.clear();
            }
            return;
        }
        if self.reconnect_job.is_some() {
            return;
        }
        // Lecture multipiste arrêtée seulement si c'est sa sortie qui a disparu.
        if lost & LOST_OUTPUT != 0 && matches!(self.transport, Transport::Playing { .. }) {
            self.stop();
        }
        let asio = self.prefs.host.as_deref() == Some("ASIO");
        // Essais suspendus (ASIO, flux refusé) puis un flux perd vraiment sa carte : on
        // repart d'un essai rapide.
        if lost != 0 && self.reconnect.is_some_and(|(at, _)| at.is_infinite()) {
            self.reconnect = None;
        }
        if self.reconnect.is_none() {
            // Flux reperdu juste après sa réouverture (ouvert, mais aucun callback) : les
            // essais restent espacés au lieu de repartir à 1 s.
            let delay = match self.last_reopen {
                Some((at, d)) if self.now - at < REOPEN_FLAP => next_delay(d, asio),
                _ => 1.0,
            };
            self.reconnect = Some((self.now + delay, delay));
            // Pilote relâché faute d'horloge : son message est déjà affiché.
            if !self.clockless {
                let what = self.engine.problems(lost).join(" · ");
                self.device_notice(if lost != 0 && self.engine.single_device() {
                    "Carte son perdue · reconnexion…".to_string()
                } else if lost != 0 {
                    format!("{what} · reconnexion…")
                } else {
                    format!("{what} · en attente…")
                });
            }
        }
        let Some((at, delay)) = self.reconnect else {
            return;
        };
        if self.now < at {
            return;
        }
        let pads_bit = self.engine.pads_slot_bit();
        let pads_busy = self.pads_status().0 > 0 && pads_bit != 0 && lost & pads_bit == 0;
        if asio {
            // ASIO : un seul pilote pour tout, rouvert d'un coup, sur ce thread (objets COM).
            if lost == 0 && self.engine.open_mask() != 0 {
                // Rien de mort, seulement un flux refusé : on ne coupe pas ce qui marche.
                self.reconnect = Some((f64::INFINITY, delay));
                let what = self.engine.problems(0).join(" · ");
                self.device_notice(format!("{what} · voir Réglages audio"));
                return;
            }
            if !self.is_stopped() || pads_busy {
                if !self.reconnect_wait {
                    self.reconnect_wait = true;
                    self.notify_level(
                        if pads_busy {
                            "Carte son perdue · réouverture à l'arrêt des pads"
                        } else {
                            "Carte son perdue · réouverture à l'arrêt"
                        },
                        Level::Device,
                    );
                }
                return;
            }
            self.reopen_all(delay);
            return;
        }
        let ctx = ReopenCtx {
            recording: self.is_recording(),
            stopped: self.is_stopped(),
            pads_playing: self.pads_status().0 > 0,
            refused: self.engine.refused(),
        };
        let slots = reopen_slots(need, &ctx, |s| self.engine.expand(s));
        if slots == 0 {
            self.reconnect = Some((self.now + delay, delay));
            return;
        }
        let job = self.engine.prepare(slots);
        // (Bits effectivement retirés, après `expand` : main rouvert avec les pads.)
        let taken = self.engine.reopening() & !self.jobs_slots();
        self.stalled &= !taken;
        // CoreAudio : sur le thread principal, comme à l'ouverture normale.
        #[cfg(target_os = "macos")]
        {
            let r = job.run();
            self.finish_reconnect(r, delay);
        }
        #[cfg(not(target_os = "macos"))]
        {
            let (tx, rx) = std::sync::mpsc::channel();
            let spawned = std::thread::Builder::new()
                .name("tracksplus-reconnect".into())
                .spawn(move || {
                    let _ = tx.send(job.run());
                });
            match spawned {
                Ok(_) => {
                    self.reconnect_job = Some(ReconnectJob {
                        since: self.now,
                        rx,
                        slots: taken,
                        alerted: false,
                    })
                }
                // (Les flux de l'essai sont fermés : ils manquent, nouvel essai plus tard.)
                Err(_) => {
                    self.engine.reopen_lost(taken);
                    self.reconnect = Some((self.now + delay, next_delay(delay, false)));
                }
            }
        }
    }

    /// Résultat de l'essai en cours et des essais bloqués. Aucun n'est jamais abandonné :
    /// un essai sans réponse depuis `REOPEN_STUCK` est signalé (en nommant le périphérique)
    /// et mis à part ; ses flux ne sont pas rouverts une seconde fois, les autres si.
    fn poll_reconnect(&mut self) {
        use std::sync::mpsc::TryRecvError;
        let delay = self.reconnect.map_or(1.0, |r| r.1);
        let mut jobs = std::mem::take(&mut self.stuck_jobs);
        jobs.extend(self.reconnect_job.take());
        for mut job in jobs {
            match job.rx.try_recv() {
                Ok(r) => self.finish_reconnect(r, delay),
                Err(TryRecvError::Disconnected) => {
                    // Essai en échec (erreur interne) : ses flux manquent, nouvel essai plus tard.
                    self.engine.reopen_lost(job.slots);
                    self.reconnect = Some((self.now + delay, next_delay(delay, false)));
                }
                Err(TryRecvError::Empty) => {
                    let stuck = self.now - job.since > REOPEN_STUCK;
                    if stuck && !job.alerted {
                        job.alerted = true;
                        let what = self.engine.slots_label(job.slots);
                        self.notify_level(
                            format!("Pilote bloqué : {what} · rebranchez la carte"),
                            Level::Warn,
                        );
                    }
                    if stuck {
                        self.stuck_jobs.push(job);
                    } else {
                        self.reconnect_job = Some(job);
                    }
                }
            }
        }
    }

    /// Installe le résultat d'un essai de réouverture partielle.
    fn finish_reconnect(&mut self, r: audio::Reopened, delay: f64) {
        let touched = r.slots();
        let rate_before = self.engine.status.sample_rate;
        self.engine.install(r);
        for (slot, hb) in self.hb.iter_mut().enumerate() {
            if touched & audio::slot_bit(slot) != 0 {
                *hb = (
                    self.engine.shared.heartbeat[slot].load(Ordering::Relaxed),
                    self.now,
                );
            }
        }
        self.reconnect_done(touched, delay, rate_before);
    }

    /// ASIO : tout le pilote rouvert d'un coup, sur ce thread, à la même fréquence qu'avant
    /// la perte (en « Auto », la carte pourrait revenir à une autre : la session serait lue
    /// à la mauvaise vitesse) ; « Auto » seulement si cette fréquence est refusée, ou si
    /// elle n'a encore jamais été ouverte (aucune carte au lancement).
    fn reopen_all(&mut self, delay: f64) {
        let rate = self.engine.status.sample_rate;
        let fixed = self.engine.rate_fixed();
        self.engine.pad(PadCmd::StopAll);
        // L'ancien pilote est libéré avant (ASIO n'en accepte qu'un à la fois).
        self.engine = Engine::offline();
        let mut prefs = self.prefs.clone();
        if fixed {
            prefs.sample_rate = prefs.sample_rate.or(Some(rate));
        }
        let open =
            |p: &AudioPrefs| Engine::open(p, &p.pad_out()).unwrap_or_else(|_| Engine::offline());
        let mut e = open(&prefs);
        if e.open_mask() == 0
            && fixed
            && self.prefs.sample_rate.is_none()
            && e.open_errors().is_some()
        {
            drop(e);
            e = open(&self.prefs);
        }
        self.engine = e;
        let now = self.now;
        self.hb = [(0, now); audio::SLOTS];
        self.open_seen = 0;
        self.stalled = 0;
        self.reconnect_done(audio::LOST_ALL, delay, rate);
    }

    /// Après un essai : soundboard et monitoring rebranchés, puis nouvel essai plus tard
    /// pour ce qui manque encore (en le disant clairement). « Reconnectée » n'est annoncé
    /// qu'au premier callback des flux rouverts (`schedule_reconnect`).
    fn reconnect_done(&mut self, touched: u8, delay: f64, rate_before: u32) {
        self.after_engine_change();
        for msg in self.engine.take_renamed() {
            self.notify(msg, false);
        }
        let lost = self.engine.shared.device_lost.load(Ordering::Acquire);
        let need = lost | self.engine.missing();
        let asio = self.prefs.host.as_deref() == Some("ASIO");
        let opened = touched & self.engine.open_mask();
        self.open_seen &= !touched;
        if opened != 0 {
            self.awaiting |= opened;
            self.announce = true;
            self.last_reopen = Some((self.now, delay));
        }
        if need == 0 {
            // Plus d'essai programmé ; si un flux rouvert ne tourne pas, le chien de garde
            // le signale et les essais reprennent, espacés (`last_reopen`).
            self.reconnect = None;
        } else {
            self.reconnect = Some((self.now + delay, next_delay(delay, asio)));
            if touched != 0 && !self.clockless {
                let what = self.engine.problems(lost).join(" · ");
                self.device_notice(format!("{what} · nouvel essai…"));
            }
        }
        // Carte revenue à une autre fréquence que la session : lecture et prise bloquées,
        // on le dit tout de suite (pas au moment d'appuyer sur Lecture en plein spectacle).
        if rate_before != self.engine.status.sample_rate
            && let Some(msg) = self.rate_mismatch()
        {
            self.notify(format!("Carte son revenue · {msg}"), true);
        }
    }

    /// Nouveau moteur ou flux rouverts : soundboard, MIDI et monitoring rebranchés (même
    /// fenêtre réduite : le retour casque des musiciens revient tout de suite).
    fn after_engine_change(&mut self) {
        self.cache.set_sample_rate(self.engine.status.sample_rate);
        self.sync_engine_to_board();
        self.sync_monitoring();
    }

    /// Message persistant sur l'état de la carte, donné une fois par situation (il ne
    /// réapparaît pas à chaque essai tant que rien ne change).
    fn device_notice(&mut self, text: String) {
        if self.device_note != text {
            self.device_note = text.clone();
            self.notify_level(text, Level::Device);
        }
    }

    /// Contrôleurs MIDI débranchés / rebranchés (même fenêtre réduite) : la liste des ports
    /// est relue en arrière-plan, le contrôleur choisi est reconnecté dès son retour.
    fn watch_midi(&mut self) {
        let want = self.board.midi_port.clone();
        let Some(change) = self.midi.watch_ports(self.now, want.as_deref()) else {
            return;
        };
        self.midi_ports = change.ports;
        if !change.lost.is_empty() {
            self.notify(
                format!("Contrôleur MIDI débranché : {}", change.lost.join(", ")),
                true,
            );
        }
        if change.reconnect {
            self.midi.connect(want.as_deref());
            let led = self.board.led_port.clone();
            self.midi.connect_leds(led.as_deref());
            // Disposition du contrôleur et LED renvoyées au prochain passage.
            self.sb.layout_port = None;
            self.sb.midi_dirty = true;
            if !self.midi.connected.is_empty() {
                self.notify(
                    format!(
                        "Contrôleur MIDI reconnecté : {}",
                        self.midi.connected.join(", ")
                    ),
                    false,
                );
            }
        } else if !change.lost.is_empty() {
            self.sb.layout_port = None;
            self.sb.midi_dirty = true;
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
        // Perte de la carte son pendant une prise (réglage de l'ordinateur, appliqué tout de
        // suite, sans rouvrir la carte).
        ui.add_space(16.0);
        ui.label(
            egui::RichText::new("PERTE DE LA CARTE SON")
                .font(t::caps(10.0))
                .color(t::TEXT_3),
        );
        ui.add_space(4.0);
        let mut auto = self.prefs.auto_restart;
        if ui
            .checkbox(
                &mut auto,
                "Relancer la prise automatiquement dès le retour de la carte",
            )
            .changed()
        {
            self.prefs.auto_restart = auto;
            self.prefs.save();
        }
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(
                "Carte débranchée, pilote relancé, horloge Dante perdue : ce qui a été capté \
                 est sauvegardé, puis une nouvelle prise démarre sur les mêmes pistes dès que \
                 la carte revient, à sa place sur la timeline. Un signal qui revient tout \
                 seul en moins de 20 s ne coupe même pas la prise.",
            )
            .color(t::TEXT_3),
        );
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
            self.notify("Copie de sécurité : choisissez un AUTRE disque", true);
        } else {
            self.set_backup_root(Some(d));
        }
    }

    fn set_backup_root(&mut self, d: Option<PathBuf>) {
        match crate::sessions::save_backup_root(d.as_deref()) {
            Ok(()) => {
                self.backup_prefs_error = None;
                self.notify(
                    match &d {
                        Some(d) => format!("Copie de sécurité activée : {}", d.display()),
                        None => "Copie de sécurité désactivée".to_string(),
                    },
                    false,
                );
            }
            // Réglage appliqué pour cette fois, mais il ne sera pas retrouvé au lancement.
            Err(e) => self.notify(format!("Réglage de la copie non enregistré : {e:#}"), true),
        }
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
        backup_session_dir_for(
            self.backup_root.as_deref()?,
            &self.project.dir,
            &self.project.name,
        )
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
                        let from = std::mem::replace(&mut job.project.dir, job.dir.clone());
                        let saved = job.project.save().is_ok();
                        backup_board(job.board, &from, &job.dir);
                        if let Some(j) = &job.journal {
                            let _ = recovery::write_journal_untracked(&job.dir, j);
                        } else if job.done && saved {
                            recovery::remove_journal(&job.dir);
                        }
                    }
                })
                .ok();
            self.backup_thread = h;
            tx
        });
        // Heure de l'état copié : le double garde celle de son contenu, même écrit plus tard
        // (second disque lent), pour départager les versions de secours (`Project::open`).
        // Pistes où la copie a ses propres fichiers (trou sur un seul des deux disques).
        let mut project = self.project.clone();
        project.mark_state_time();
        let here = self.project.dir.clone();
        let has_clip =
            |p: &Project, id: u64| p.tracks.iter().any(|t| t.clips.iter().any(|c| c.id == id));
        self.backup_alt.retain(|a| {
            a.dir == here && (a.main.is_empty() || a.main.iter().any(|m| has_clip(&project, m.id)))
        });
        apply_backup_alts(&mut project, &self.backup_alt);
        let _ = tx.send(BackupJob {
            project,
            dir,
            journal,
            done,
            board: self.board.clone(),
        });
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
                format!("Prise illisible, jouée en silence : {first}{more}"),
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
            alerts.push("Écriture disque arrêtée · arrêtez la prise".into());
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
            // La prise reste ouverte : si le signal revient, le trou est comblé par du
            // silence et la suite reste à sa place ; sinon arrêt (et relance au retour).
            let left = (REC_STALL_STOP - (self.now - self.rec_watch.1))
                .max(0.0)
                .ceil();
            alerts.push(format!(
                "PLUS DE SIGNAL{} · arrêt dans {left:.0} s",
                if dante { " · horloge Dante ?" } else { "" }
            ));
        }
        let gap = self.engine.shared.rec_gap_frames.load(Ordering::Relaxed);
        if gap > 0 {
            alerts.push(format!(
                "Entrée coupée · {} de silence inséré",
                fmt_gap(gap as f64 / self.sr() as f64)
            ));
        }
        // Carte son d'abord : c'est l'alerte la plus grave, elle reste toujours visible.
        if let Some(e) = self.engine.shared.error_text() {
            alerts.push(format!("Carte son : {e}"));
        }
        let bps = self.rec_bytes_per_sec();
        if let Some(free) = self.disk_free {
            let secs = free / bps;
            // Alerte dès 30 min restantes (comme les enregistreurs pro) : le temps de
            // libérer de la place ou de prévoir un autre disque avant la fin du concert.
            if secs < 1800 {
                alerts.push(format!(
                    "Disque presque plein · ≈ {} restant",
                    fmt_duration(secs)
                ));
            }
        }
        match self.backup_free {
            Some(Ok(free)) if free / bps < 1800 => alerts.push(format!(
                "Copie de sécurité : second disque presque plein · ≈ {} restant",
                fmt_duration(free / bps)
            )),
            Some(Err(())) => alerts.push("Copie de sécurité : second disque introuvable".into()),
            _ => {}
        }
        let ov = self.engine.shared.overruns.load(Ordering::Relaxed);
        if ov > 0 {
            alerts.push(format!("{ov} pertes de données · disque trop lent"));
        }
        if self.engine.shared.copy_overruns.load(Ordering::Relaxed) > 0 {
            alerts.push("Copie de sécurité : second disque trop lent".into());
        }
        // Erreurs de fichiers en dernier, regroupées : recalculées seulement quand elles
        // changent (jamais une liste reformatée à chaque image).
        if let Some(r) = &self.recorder {
            let g = r.errors_gen();
            if self.disk_err_cache.as_ref().is_none_or(|(k, _)| *k != g) {
                let errs = r.errors();
                let txt = match errs.len() {
                    0 => None,
                    1 => Some(format!("Erreur disque : {}", errs[0])),
                    n => Some(format!(
                        "Erreurs disque ({n}) : {} (+{} autre(s))",
                        errs[0],
                        n - 1
                    )),
                };
                self.disk_err_cache = Some((g, txt));
            }
            if let Some((_, Some(t))) = &self.disk_err_cache {
                alerts.push(t.clone());
            }
        }
        alerts
    }

    fn update_playhead(&mut self) {
        let sh = &self.engine.shared;
        match self.transport {
            Transport::Playing { .. } => {
                self.playhead = self.heard_at(sh.out_frames.load(Ordering::Relaxed))
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
        // Repère posé pendant une prise : sauvegardé sous 10 s (sauvegarde automatique en
        // arrière-plan), pour qu'un crash en plein concert ne fasse pas perdre le repérage.
        if self.is_recording() {
            self.last_autosave = self.last_autosave.min(self.now - 20.0);
        }
        let n = self.project.markers.len();
        self.notify(format!("Repère {n} · {}", fmt_time(pos, self.sr())), false);
    }

    // ---------------------------------------------------------------- montage

    /// Mémorise l'état avant une modification (pour Annuler).
    pub fn checkpoint(&mut self) {
        self.undo.push(Snapshot {
            tracks: self.project.tracks.clone(),
            markers: self.project.markers.clone(),
            next_id: self.project.next_id,
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
            next_id: self.project.next_id,
        };
        let current = std::mem::take(&mut self.project.tracks);
        self.project.tracks = project::merge_clips(current, snap.tracks, snap.next_id);
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
    /// déjà sur le disque (préparation des pistes et entrées avant un concert). Jamais une
    /// session en lecture seule (fichier d'une version plus récente) : la sauvegarde
    /// automatique ne la réécrit pas, et ne répète pas son refus toutes les 30 s.
    fn worth_saving(&self) -> bool {
        self.project.read_only.is_none()
            && (self.project.has_audio() || self.project.file_path().exists())
    }

    pub fn autosave(&mut self) {
        self.settle_autosave();
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
            self.notify(format!("Réglages soundboard non sauvegardés : {e:#}"), true);
        }
        // Fichier de soundboard protégé (illisible, ou d'une version plus récente) : rien
        // n'est écrit tant que le soundboard n'a pas vraiment changé ; à la première vraie
        // modification, l'ancien fichier est mis de côté sous un autre nom, jamais écrasé.
        if self.board.guard.is_some() {
            if !self.board.changed_since_guard() {
                return true;
            }
            match self.board.release_guard(&self.project.dir) {
                Ok(Some(aside)) => self.notify(
                    format!(
                        "Ancien soundboard gardé : « {} »",
                        aside.file_name().unwrap_or_default().to_string_lossy()
                    ),
                    true,
                ),
                Ok(None) => {}
                Err(e) => {
                    self.notify(format!("Soundboard non sauvegardé : {e:#}"), true);
                    self.board_changed = Some(self.now + 29.0);
                    return false;
                }
            }
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
        // Sauvegarde préparée pour la fin de session : plus ancienne, elle ne doit pas
        // remplacer celle-ci (refaite au prochain passage s'il y a encore à sauvegarder).
        crate::shutdown::set_pending_board_save(None);
        self.pending_saves_at = None;
        true
    }

    /// Avant d'ouvrir ou de créer une autre session : tout est sauvegardé, sinon on
    /// n'abandonne pas la session en cours (disque plein, clé retirée…). Un second essai
    /// dans les 10 s passe outre, pour ne jamais rester bloqué.
    pub fn save_before_leaving(&mut self) -> bool {
        self.save_before("Session non sauvegardée · recommencez pour forcer")
    }

    /// Sauvegarde automatique en cours : on attend qu'elle soit écrite (10 s au plus) ; si
    /// elle a échoué ou traîne, la session redevient « à sauvegarder » (sauvegarde normale
    /// juste après, par l'appelant).
    fn settle_autosave(&mut self) {
        self.settle_autosave_within(std::time::Duration::from_secs(10));
    }

    /// Comme `settle_autosave`, en attendant au plus `timeout`.
    fn settle_autosave_within(&mut self, timeout: std::time::Duration) {
        if let Some((dir, rx)) = self.autosave_job.take() {
            let ok = matches!(rx.recv_timeout(timeout), Ok(Ok(())));
            if !ok && dir == self.project.dir {
                self.project.dirty = true;
            }
        }
    }

    /// Comme `save_before_leaving`, avec le message à afficher en cas d'échec.
    pub fn save_before(&mut self, refused: &str) -> bool {
        self.settle_autosave();
        let mut ok = true;
        if self.project.dirty
            && self.worth_saving()
            && let Err(e) = self.project.save()
        {
            self.project.dirty = true;
            ok = false;
            self.notify(format!("Sauvegarde de la session impossible : {e:#}"), true);
        } else if self.project.dirty
            && let Some(why) = &self.project.read_only
        {
            // Session en lecture seule modifiée : ses modifications seraient perdues sans
            // prévenir.
            let msg = format!("Modifications non enregistrées : {why}");
            ok = false;
            self.notify(msg, true);
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

    /// Sons sur un partage réseau imposés par un fichier : marqués indisponibles sans être
    /// ouverts (sécurité, voir `Board::load_session`).
    fn park_network_pads(&mut self, paths: &[PathBuf]) {
        for p in paths {
            self.cache.park(p);
        }
    }

    /// Soundboard de la session qui vient d'être ouverte ou créée (les réglages du
    /// contrôleur restent ceux de l'ordinateur). Renvoie l'avertissement à afficher.
    fn load_session_board(&mut self) -> Option<String> {
        self.engine.pad(PadCmd::StopAll);
        let fresh = !self
            .project
            .dir
            .join(crate::soundboard::BOARD_FILE)
            .exists();
        let mut warnings = Vec::new();
        // Fichier illisible : soundboard vide affiché, mais le fichier est protégé (jamais
        // écrasé par ce soundboard vide, ni à la fermeture ni sur le second disque).
        let mut unreadable = None;
        let mut b = match Board::load_session(&self.project.dir) {
            Ok(b) => b,
            Err(e) => {
                unreadable = Some(format!("soundboard illisible : {e:#}"));
                Board::default()
            }
        };
        b.take_devices(&self.board);
        let network = b.network_pads.clone();
        if !network.is_empty() {
            self.park_network_pads(&network);
            warnings.push(format!(
                "{} son(s) hors session bloqué(s) · à remplacer",
                network.len()
            ));
        }
        if fresh {
            // Nouvelle session : même taille de grille que jusqu'ici.
            b.cols = self.board.cols;
            b.rows = self.board.rows;
        }
        if let Some(why) = unreadable {
            b.protect(why);
        }
        if let Some(g) = &b.guard {
            warnings.insert(0, format!("{} · fichier protégé", g.reason));
        }
        self.set_board(b);
        (!warnings.is_empty()).then(|| warnings.join(" · "))
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
        // Fichier reçu d'ailleurs : jamais de partage réseau ni de fichier hors de son
        // dossier ouvert (ou copié) tout seul.
        b.resolve_paths(&dir);
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
                Err("Import du soundboard interrompu".to_string())
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
        // Fichier de la session protégé : il reste protégé, donc mis de côté (pas écrasé)
        // par l'enregistrement du soundboard importé.
        b.guard = self.board.guard.take();
        let network = b.network_pads.clone();
        self.park_network_pads(&network);
        self.set_board(b);
        self.board_changed = Some(self.now);
        self.save_board_now();
        if missing.is_empty() {
            self.notify("Soundboard importé dans la session", false);
        } else {
            self.notify(
                format!(
                    "Soundboard importé · {} son(s) introuvable(s)",
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

    /// Versions de secours d'une session, en dernier recours si son fichier est abîmé : son
    /// double sur le second disque (et les versions précédentes de ce double).
    fn session_fallbacks(&self, path: &Path) -> Vec<PathBuf> {
        let (Some(root), Some(dir), Some(name)) = (
            self.backup_root.as_deref(),
            path.parent(),
            path.file_stem().map(|s| s.to_string_lossy().into_owned()),
        ) else {
            return Vec::new();
        };
        let Some(double) = backup_session_dir_for(root, dir, &name) else {
            return Vec::new();
        };
        let file = double.join(format!("{name}.{PROJECT_EXT}"));
        let mut v = vec![file.clone()];
        v.extend(project::backup_paths(&file));
        v
    }

    /// Charge une session (et récupère les prises d'un enregistrement interrompu). Un
    /// fichier abîmé est remplacé par sa version précédente (voir `Project::open`). Renvoie
    /// `true` si la session est ouverte.
    pub fn load_session(&mut self, path: &Path) -> bool {
        let fallbacks = self.session_fallbacks(path);
        let (mut p, restored) = match Project::open(path, &fallbacks) {
            Ok(r) => r,
            Err(e) => {
                self.notify(format!("{e:#}"), true);
                return false;
            }
        };
        // Prises de cette session encore en cours d'écriture (session quittée puis rouverte
        // pendant qu'un disque lent finissait) : jamais touchées ici, elles seront intégrées
        // à la fin de leur écriture.
        let live = journals_of(
            self.stalled_takes
                .iter()
                .chain(self.finishing.as_ref())
                .map(|f| &f.take),
            &p.dir,
        );
        let recovered = recovery::recover_with(&mut p, &live);
        self.start_screen = false;
        self.project = p;
        // Forme d'onde en direct d'une prise d'une autre session (encore en écriture) : elle
        // ne s'affiche pas sur les pistes de celle-ci (elle reste dans sa fin de prise).
        if self
            .finishing
            .as_ref()
            .is_none_or(|f| f.take.dir != self.project.dir)
        {
            self.live.clear();
        }
        let board_warning = self.load_session_board();
        self.reset_view_for_session();
        self.load_peaks();
        recovery::remember_session(&self.project);
        // Avertissements (tous gardés : un seul message s'affiche à la fois).
        let mut warnings: Vec<String> = Vec::new();
        if let Some(m) = restored {
            // Version précédente reprise : réécrite tout de suite à la place du fichier abîmé.
            if self.project.dirty
                && let Err(e) = self.project.save()
            {
                warnings.push(format!("Sauvegarde de la session impossible : {e:#}"));
            }
            warnings.insert(0, m);
        }
        if let Some(why) = &self.project.read_only {
            warnings.push(format!("Lecture seule : {why}"));
        }
        let mut info = None;
        match recovered {
            Ok(r) if r.takes > 0 || r.saved.is_err() => {
                let n = r.takes;
                self.zoom_to_fit();
                match r.saved {
                    Ok(()) => info = Some(format!("{n} prise(s) interrompue(s) récupérée(s)")),
                    Err(e) => warnings.push(format!(
                        "{n} prise(s) interrompue(s) récupérée(s) · session non sauvegardée : {e:#}"
                    )),
                }
            }
            Err(e) => warnings.push(format!("Récupération incomplète : {e:#}")),
            _ => {
                if self.project.has_audio()
                    && self.project.sample_rate != self.engine.status.sample_rate
                {
                    warnings.push(format!(
                        "Session en {} kHz, carte en {} kHz",
                        fmt_khz(self.project.sample_rate),
                        fmt_khz(self.engine.status.sample_rate)
                    ));
                }
            }
        }
        warnings.extend(board_warning);
        if !warnings.is_empty() {
            if let Some(i) = info {
                warnings.insert(0, i);
            }
            self.notify(warnings.join(" · "), true);
        } else {
            let msg = info.unwrap_or_else(|| format!("« {} » ouverte", self.project.name));
            self.notify(msg, false);
        }
        true
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
        // Même dossier que la session actuelle : rien ne serait copié, ce n'est pas une
        // nouvelle session (on ne prétend pas le contraire).
        let same = |a: &Path, b: &Path| {
            let c = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
            c(a) == c(b)
        };
        if same(&dir, &self.project.dir) || same(&parent, &self.project.dir) {
            self.notify("Dossier de la session · choisissez-en un autre", true);
            return;
        }
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
                Err("Enregistrer sous interrompu".to_string())
            }
        };
        self.save_as_job = None;
        match res {
            Ok((p, b, missing, lost)) => {
                if !lost.is_empty() {
                    self.notify(
                        format!(
                            "{} prise(s) introuvable(s) : {}",
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
                // Nouveau dossier : aucun fichier protégé à y garder, le soundboard (même
                // celui d'un fichier illisible ou plus récent) y est enregistré normalement.
                self.board.guard = None;
                self.board_changed = Some(self.now);
                self.save_board_now();
                recovery::remember_session(&self.project);
                // (Les avertissements ci-dessus restent affichés.)
                if lost.is_empty() && missing.is_empty() {
                    self.notify(
                        format!("Session enregistrée · {}", self.project.dir.display()),
                        false,
                    );
                }
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
        let board_warning = self.load_session_board();
        self.peaks.clear();
        self.reset_view_for_session();
        recovery::remember_session(&self.project);
        match board_warning {
            Some(w) => self.notify(w, true),
            None => self.notify(format!("Nouvelle session « {} »", self.project.name), false),
        }
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
        let (base, n) = (self.in_base(), self.engine.status.in_channels);
        (base..base + n)
            .step_by(step)
            .find(|&i| (i..i + step as u16).all(|c| !used.contains(&c)))
            .or((n > 0).then_some(base))
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

    pub fn apply_audio(&mut self, mut prefs: AudioPrefs) -> Result<(), String> {
        // Réglé à part (onglet Enregistrement), appliqué tout de suite : jamais écrasé ici.
        prefs.auto_restart = self.prefs.auto_restart;
        if self.editing_locked() {
            return Err("Arrêtez la prise d'abord".into());
        }
        // Choix explicite de l'utilisateur : une reconnexion automatique en cours s'efface.
        // Si elle est en train d'ouvrir la carte, on attend qu'elle ait fini (et on jette
        // son résultat) : jamais deux ouvertures du même périphérique en même temps. Un
        // pilote qui ne répond plus du tout : on ne l'ouvre pas une seconde fois.
        let blocked = |e: &Engine, slots: u8| {
            format!(
                "Pilote bloqué : {} · rebranchez la carte",
                e.slots_label(slots)
            )
        };
        if let Some(j) = self.stuck_jobs.first() {
            return Err(blocked(&self.engine, j.slots));
        }
        if let Some(j) = &self.reconnect_job {
            match j.rx.recv_timeout(std::time::Duration::from_secs(5)) {
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    return Err(blocked(&self.engine, j.slots));
                }
                // (Résultat jeté ici : ses flux sont fermés.)
                _ => self.reconnect_job = None,
            }
        }
        self.stop();
        self.reconnect = None;
        self.reconnect_wait = false;
        self.device_note.clear();
        self.stalled = 0;
        self.awaiting = 0;
        self.announce = false;
        self.last_reopen = None;
        self.open_seen = 0;
        self.not_started = 0;
        self.clockless = false;
        self.engine.pad(PadCmd::StopAll);
        // Libère d'abord l'ancien pilote (ASIO n'en autorise qu'un à la fois).
        self.engine = Engine::offline();
        // Un flux refusé n'empêche pas les autres : la carte tourne avec ce qui s'ouvre, et
        // l'erreur reste affichée dans les réglages pour être corrigée.
        // Rien du tout ne s'ouvre avec les nouveaux réglages : on revient aux anciens.
        let open =
            |p: &AudioPrefs| Engine::open(p, &p.pad_out()).unwrap_or_else(|_| Engine::offline());
        let engine = open(&prefs);
        let res = engine.open_errors().map_or(Ok(()), Err);
        if engine.open_mask() == 0 && res.is_err() {
            drop(engine);
            self.engine = open(&self.prefs);
        } else {
            self.engine = engine;
            self.prefs = prefs;
            self.prefs.save();
        }
        if !self.project.has_audio() {
            self.project.sample_rate = self.engine.status.sample_rate;
        }
        let now = self.now;
        self.hb = [(0, now); audio::SLOTS];
        self.out_devices = audio::list_devices(self.prefs.host.as_deref()).1;
        self.after_engine_change();
        res
    }

    /// « Actualiser » : rouvre la carte son pour relire les réglages du pilote. Beaucoup de
    /// pilotes (RME notamment) ne donnent un buffer ou une fréquence changés dans leur
    /// panneau qu'à la réouverture. En ASIO, le panneau du pilote fait foi : buffer et
    /// fréquence repassent en « Auto ».
    fn reload_audio(&mut self) {
        if self.is_recording() {
            self.notify("Arrêtez la prise d'abord", true);
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
                    "Carte son relancée · {} kHz",
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

    /// Interroge tous les pilotes ASIO (nombre de canaux, paires de sorties, fréquences) :
    /// le pilote en service est fermé le temps de la lecture (une application n'en charge
    /// qu'un à la fois), puis la carte est rouverte avec les réglages en cours.
    fn probe_asio(&mut self, dlg: &mut SettingsDialog) {
        if dlg.prefs.host.as_deref() != Some("ASIO") || self.asio_in_use().is_none() {
            // Hors ASIO, ou aucun pilote ASIO en service : tous répondent déjà.
            dlg.asio_in_use = None;
            dlg.refresh();
            return;
        }
        if self.editing_locked() || !self.stuck_jobs.is_empty() || self.reconnect_job.is_some() {
            // Jamais pendant une prise ni pendant une réouverture de la carte.
            dlg.refresh();
            return;
        }
        let prefs = self.prefs.clone();
        self.engine.pad(PadCmd::StopAll);
        self.engine = Engine::offline();
        dlg.asio_in_use = None;
        dlg.refresh();
        if let Err(e) = self.apply_audio(prefs) {
            dlg.error = Some(e);
        }
        dlg.asio_in_use = self.asio_in_use();
    }

    pub fn open_export(&mut self) {
        if self.editing_locked() {
            self.notify("Export impossible pendant la prise", true);
            return;
        }
        if !self.project.has_audio() {
            self.notify("Rien à exporter", true);
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
        self.settings_dlg = Some(SettingsDialog::new(&self.prefs, self.asio_in_use()));
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
        // Échap = « Annuler » des confirmations.
        self.confirm_close = false;
        self.sb.confirm_delete_page = None;
    }

    /// Fichier du journal des incidents de la session ouverte (voir `incident`).
    fn incidents_path(&self) -> Option<PathBuf> {
        let dir = if self.start_screen || self.project.read_only.is_some() {
            dirs::config_dir()?.join("TracksPlus")
        } else {
            self.project.dir.clone()
        };
        Some(dir.join(crate::incidents::FILE))
    }

    /// Ouvre la fenêtre du journal des incidents (lignes les plus récentes d'abord).
    pub fn open_incidents(&mut self) {
        let lines = self
            .incidents_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|t| {
                let mut v: Vec<String> = t.lines().map(str::to_string).collect();
                v.reverse();
                v.truncate(INCIDENTS_SHOWN);
                v
            })
            .unwrap_or_default();
        self.incidents_dlg = Some(lines);
    }

    fn incidents_dialog(&mut self, ctx: &egui::Context) {
        let Some(lines) = &self.incidents_dlg else {
            return;
        };
        let mut close = false;
        let mut open_file = false;
        egui::Modal::new(egui::Id::unique("incidents_dlg"))
            .frame(t::window_frame())
            .show(ctx, |ui| {
                ui.set_width(640.0);
                ui.label(
                    egui::RichText::new("JOURNAL DES INCIDENTS")
                        .font(t::semibold(12.0))
                        .color(t::TEXT_2),
                );
                ui.add_space(8.0);
                if lines.is_empty() {
                    ui.label(egui::RichText::new("Aucun incident").color(t::TEXT_3));
                } else {
                    egui::ScrollArea::vertical()
                        .max_height(380.0)
                        .show(ui, |ui| {
                            for l in lines {
                                // « AAAA-MM-JJ HH:MM:SS  texte » : l'heure en retrait.
                                let (when, what) = l.split_once("  ").unwrap_or(("", l.as_str()));
                                ui.horizontal_wrapped(|ui| {
                                    ui.label(
                                        egui::RichText::new(when)
                                            .font(egui::FontId::monospace(11.0))
                                            .color(t::TEXT_3),
                                    );
                                    ui.label(egui::RichText::new(what).font(t::medium(12.0)));
                                });
                            }
                        });
                }
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.button("Ouvrir le fichier").clicked() {
                        open_file = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if w::primary_button(ui, None, "Fermer", true).clicked() {
                            close = true;
                        }
                    });
                });
            });
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            close = true;
        }
        if open_file && let Some(p) = self.incidents_path() {
            if p.is_file() {
                open_in_file_manager(&p);
            } else {
                self.notify("Aucun incident", false);
            }
        }
        if close {
            self.incidents_dlg = None;
        }
    }

    pub fn dialog_open(&self) -> bool {
        self.incidents_dlg.is_some()
            || self.settings_dlg.is_some()
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
        let base = self.in_base();
        for t in self.project.tracks.iter().filter(|t| t.monitor) {
            if let Some(i) = t.engine_input(base) {
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
        // Fader master : lu par le callback de sortie, entendu aussitôt.
        let master = self.project.master_gain().to_bits();
        if sh.master_gain.load(Ordering::Relaxed) != master {
            sh.master_gain.store(master, Ordering::Relaxed);
        }
    }

    /// Fader ou panoramique d'une piste changé : la session est à sauvegarder, et la
    /// lecture en cours prend le nouveau mix (voir `flush_mix`).
    pub fn mix_changed(&mut self) {
        self.project.dirty = true;
        self.mix_dirty = true;
    }

    /// Mix changé pendant la lecture : nouveau rendu (comme mute / solo), au plus toutes
    /// les 0,12 s pendant qu'on bouge un fader (le disque n'est pas relu à chaque image).
    fn flush_mix(&mut self) {
        // Boucle activée, coupée ou plage changée pendant la lecture : nouveau rendu.
        if matches!(self.transport, Transport::Playing { .. })
            && self.active_loop() != self.play_loop
        {
            self.mix_dirty = true;
        }
        if self.mix_dirty && self.now - self.mix_switch_at >= MIX_SWITCH_EVERY {
            self.mix_dirty = false;
            self.mix_switch_at = self.now;
            self.update_audible();
        }
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
            // Toute piste qui a des prises : niveau de lecture (après son fader).
            if t.clips.is_empty() {
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
        // déconnecté ne fige jamais l'interface. Disque de la session et second disque
        // mesurés à part : un second disque bloqué ne fige jamais la mesure du principal.
        // Une seule mesure en cours par disque, jamais abandonnée tant que la session ne
        // change pas (un disque qui ne répond plus n'accumule pas de threads bloqués) : au
        // bout de 10 s, il est signalé comme ne répondant plus.
        let main_dir = Some(self.project.dir.clone());
        let backup_dir = self.backup_session_dir();
        let poll = |job: &mut Option<(f64, PathBuf, std::sync::mpsc::Receiver<Option<u64>>)>,
                    want: &Option<PathBuf>|
         -> DiskPoll {
            use std::sync::mpsc::TryRecvError;
            let Some((since, dir, rx)) = job else {
                return DiskPoll::Idle;
            };
            if want.as_ref() != Some(dir) {
                // Session (ou second disque) changé : la mesure de l'ancien ne compte plus.
                *job = None;
                return DiskPoll::Idle;
            }
            match rx.try_recv() {
                Ok(free) => {
                    *job = None;
                    DiskPoll::Done(free)
                }
                Err(TryRecvError::Disconnected) => {
                    *job = None;
                    DiskPoll::Done(None)
                }
                Err(TryRecvError::Empty) if now - *since > 10.0 => DiskPoll::Stuck,
                Err(TryRecvError::Empty) => DiskPoll::Busy,
            }
        };
        match poll(&mut self.disk_job, &main_dir) {
            DiskPoll::Done(free) => self.disk_free = free,
            // Valeur inconnue plutôt que périmée (l'alerte « disque presque plein » ne
            // s'appuie jamais sur une ancienne mesure).
            DiskPoll::Stuck => self.disk_free = None,
            _ => {}
        }
        match poll(&mut self.backup_disk_job, &backup_dir) {
            DiskPoll::Done(free) => self.backup_free = Some(free.ok_or(())),
            DiskPoll::Stuck => self.backup_free = Some(Err(())),
            _ => {}
        }
        if backup_dir.is_none() {
            self.backup_free = None;
        }
        if now - self.disk_checked > 2.0 {
            self.disk_checked = now;
            let spawn = |dir: PathBuf| {
                let (tx, rx) = std::sync::mpsc::channel();
                let d = dir.clone();
                std::thread::Builder::new()
                    .name("tracksplus-disk-space".into())
                    .spawn(move || {
                        let _ = tx.send(free_space(&d));
                    })
                    .ok()
                    .map(|_| (now, dir, rx))
            };
            if self.disk_job.is_none() {
                self.disk_job = spawn(self.project.dir.clone());
            }
            if self.backup_disk_job.is_none()
                && let Some(b) = backup_dir
            {
                self.backup_disk_job = spawn(b);
            }
        }
    }

    fn update_title(&mut self, ctx: &egui::Context) {
        let tt = if self.start_screen {
            "TracksPlus".to_string()
        } else {
            format!(
                "{}{}{} — TracksPlus",
                self.project.name,
                if self.project.read_only.is_some() {
                    " (lecture seule)"
                } else {
                    ""
                },
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
        // État de la carte : vert = le son tourne ; orange = carte choisie mais en attente
        // (pas d'horloge Dante, reconnexion) ; rouge = aucune carte.
        let waiting = self.card_waiting();
        let st = &self.engine.status;
        let ok = st.input.is_some() || st.output.is_some();
        let dev = st
            .input
            .clone()
            .or(st.output.clone())
            .unwrap_or_else(|| "Aucune carte son".into());
        let dev = match waiting {
            Some(w) if ok => format!("{dev} · {w}"),
            _ => dev,
        };
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
            match (ok, waiting) {
                (false, _) => t::REC,
                (true, Some(_)) => t::ORANGE,
                (true, None) => t::PLAY,
            },
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
        // (À l'arrêt, Stop annule aussi une relance automatique de prise en attente.)
        if w::icon_button(
            ui,
            Icon::Stop,
            30.0,
            BtnStyle::Ghost,
            !stopped || self.resume.is_some(),
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
        // Relance automatique en attente : visible tant qu'elle l'est, annulable d'un clic.
        if self.resume.is_some() {
            ui.add_space(8.0);
            let blink = (self.now * 1.6).fract() < 0.55;
            let text = egui::RichText::new("RELANCE DE LA PRISE EN ATTENTE")
                .font(t::semibold(10.5))
                .color(if blink { t::REC } else { t::TEXT_2 });
            if ui
                .add(egui::Button::new(text).fill(t::INSET))
                .on_hover_text(
                    "La prise a été arrêtée par une perte de la carte son : elle repartira toute seule dès son retour. Cliquer pour annuler.",
                )
                .clicked()
            {
                self.cancel_resume();
            }
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
        if ui.button("Journal des incidents").clicked() {
            ui.close();
            self.open_incidents();
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
            if let Some(e) = sh.error_text() {
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
                    // (Réglage appliqué tout de suite : le dialogue suit.)
                    dlg.prefs.auto_restart = self.prefs.auto_restart;
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
                                dlg.card = None;
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
                    let asio = dlg.asio();

                    // ---- Carte son : celles du pilote (en ASIO, chaque pilote est une carte ;
                    // en WASAPI, les périphériques de Windows sont regroupés par carte). Entrée
                    // et sorties ne proposent ensuite que les siennes.
                    ui.label(egui::RichText::new("Carte son").color(t::TEXT_2));
                    ui.horizontal(|ui| {
                        let none = if asio { "Aucune" } else { "Par défaut (Windows)" };
                        let cur = dlg.card.clone().unwrap_or_else(|| none.to_string());
                        let mut pick: Option<Option<String>> = None;
                        egui::ComboBox::from_id_salt("card")
                            .width(combo_w)
                            .selected_text(combo_text(ui, &cur, combo_w))
                            .show_ui(ui, |ui| {
                                if !asio && ui.selectable_label(dlg.card.is_none(), none).clicked() {
                                    pick = Some(None);
                                }
                                for c in dlg.cards() {
                                    if ui.selectable_label(dlg.card.as_deref() == Some(&c), &c).clicked() {
                                        pick = Some(Some(c));
                                    }
                                }
                            });
                        if let Some(c) = pick {
                            dlg.select_card(c);
                        }
                        // Carte branchée, Dante Virtual Soundcard passée en ASIO… après
                        // l'ouverture des réglages. ASIO : tous les pilotes sont interrogés
                        // (celui en service est fermé un instant).
                        if ui.small_button("Actualiser").clicked() {
                            if asio {
                                dlg.probe = true;
                            } else {
                                dlg.refresh();
                            }
                        }
                    });
                    ui.end_row();

                    ui.label(egui::RichText::new("Entrée").color(t::TEXT_2));
                    let ins = dlg.card_ins();
                    if asio {
                        // ASIO : l'entrée, c'est la carte (les canaux se choisissent par piste).
                        let txt = match ins.first() {
                            Some(d) if d.channels > 0 => format!("{} entrées", d.channels),
                            Some(_) => "—".to_string(),
                            None => "Aucune entrée".to_string(),
                        };
                        ui.label(txt);
                    } else {
                        let cur = dlg.prefs.input.clone().unwrap_or_else(|| "Par défaut (Windows)".into());
                        let shown = combo_text(ui, &cur, combo_w);
                        // Windows découpe la carte en paires et WASAPI n'en ouvre qu'une.
                        let pairs = dlg.card.is_some() && ins.len() > 1;
                        ui.horizontal(|ui| {
                        egui::ComboBox::from_id_salt("in").width(combo_w).selected_text(shown).show_ui(ui, |ui| {
                            if (dlg.card.is_none() || ins.is_empty())
                                && ui.selectable_label(dlg.prefs.input.is_none(), "Par défaut (Windows)").clicked()
                            {
                                dlg.prefs.input = None;
                            }
                            for d in &ins {
                                let txt = if d.channels > 0 {
                                    format!("{}   {} canaux", d.name, d.channels)
                                } else {
                                    d.name.clone()
                                };
                                if ui.selectable_label(dlg.prefs.input.as_deref() == Some(&d.name), txt).clicked() {
                                    dlg.prefs.input = Some(d.name.clone());
                                }
                            }
                        });
                        if pairs {
                            ui.label(egui::RichText::new("ASIO : toutes les entrées").color(t::TEXT_3));
                        }
                        });
                    }
                    ui.end_row();

                    // ---- Sorties de la carte (multipiste, soundboard, monitoring).
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new("SORTIES").font(t::caps(10.0)).color(t::TEXT_3));
                    ui.label("");
                    ui.end_row();
                    let outs = dlg.card_outs();
                    let default_out = format!(
                        "Par défaut{}",
                        dlg.default_out.as_ref().map_or(String::new(), |n| format!(" · {n}"))
                    );
                    // « Par défaut (Windows) » seulement sans carte choisie.
                    let main_none = dlg.card.is_none().then_some("Par défaut (Windows)");
                    ui.label(egui::RichText::new("Main multipiste").color(t::TEXT_2))
                        .on_hover_text("Lecture des pistes");
                    dlg.probe |= dest_combo(ui, "out_main", combo_w, &outs, &mut dlg.prefs.output, &mut dlg.prefs.out_left, main_none, &default_out);
                    ui.end_row();
                    ui.label(egui::RichText::new("Monitoring multipiste").color(t::TEXT_2))
                        .on_hover_text("Écoute en direct des entrées (bouton casque des pistes)");
                    dlg.probe |= dest_combo(ui, "out_mon", combo_w, &outs, &mut dlg.prefs.monitor_output, &mut dlg.prefs.monitor_left, Some("Comme le main multipiste"), "Comme le main multipiste");
                    ui.end_row();
                    ui.label(egui::RichText::new("Soundboard").color(t::TEXT_2))
                        .on_hover_text("Lecture des pads");
                    dlg.probe |= dest_combo(ui, "out_pads", combo_w, &outs, &mut dlg.prefs.pad_output, &mut dlg.prefs.pad_left, Some("Comme le main multipiste"), "Comme le main multipiste");
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
        if std::mem::take(&mut dlg.probe) {
            self.probe_asio(&mut dlg);
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
            self.notify("Fermez la fenêtre ouverte d'abord", true);
            return;
        }
        if self.editing_locked() {
            self.notify("Import impossible pour l'instant", true);
            return;
        }
        let (audio, other): (Vec<PathBuf>, Vec<PathBuf>) = paths
            .into_iter()
            .partition(|p| crate::soundboard::is_audio_file(p));
        if audio.is_empty() {
            self.notify(
                format!(
                    "Format non reconnu : {}",
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
                // En minuscules, comme `new_take_file_avoiding` le compare (Windows ne
                // distingue pas « Intro_001.wav » et « intro_001.wav »).
                taken.insert(file.to_lowercase());
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
                // Import long : un fichier par tranche de 2 Go, posés bout à bout (comme les
                // segments d'une prise).
                let mut start = at;
                for (file, frames) in i.segments {
                    let cid = self.project.alloc_id();
                    if let Some(t) = self.project.track_mut(tid) {
                        t.clips.push(Clip {
                            id: cid,
                            file,
                            start,
                            offset: 0,
                            len: frames,
                            src_len: frames,
                            fade_in: 0,
                            fade_out: 0,
                        });
                    }
                    start += frames;
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
                    "{}{} fichier(s) non importé(s) : {}",
                    if n > 0 {
                        format!("{n} piste(s) ajoutée(s) · ")
                    } else {
                        String::new()
                    },
                    errors.len(),
                    join_few(&errors, 3)
                ),
                true,
            );
        } else if cancelled {
            self.notify("Import annulé", false);
        } else if n > 0 {
            self.notify(format!("{n} fichier(s) importé(s)"), false);
        }
    }

    fn new_tracks_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut d) = self.tracks_dlg.take() else {
            return;
        };
        let in_ch = self.engine.status.in_channels;
        let in_base = self.in_base();
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
                            in_base,
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
                        if last > (in_base + in_ch) as u32 {
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
                                    "Export terminé · {} fichier(s) · {}",
                                    out.files.len(),
                                    join_few(&out.warnings, 3)
                                ),
                                true,
                            );
                        }
                        if let Some(d) = dir {
                            open_in_file_manager(&d);
                        }
                    }
                    Err(e) if e.is::<crate::export::Cancelled>() => {
                        self.notify("Export annulé", false)
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
        let mut hovered = false;
        // Sous les fenêtres (réglages, export…) : un message ne cache jamais leurs boutons.
        egui::Area::new(egui::Id::unique("toast"))
            .anchor(Align2::CENTER_BOTTOM, vec2(0.0, -48.0))
            .order(egui::Order::Middle)
            .interactable(true)
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
                        for (level, text) in &tt.items {
                            let col = if *level == Level::Critical {
                                t::REC
                            } else {
                                t::TEXT
                            };
                            ui.label(
                                egui::RichText::new(text)
                                    .font(t::medium(12.0))
                                    .color(col.gamma_multiply(fade)),
                            );
                        }
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
                // Clic : fermé tout de suite. Souris dessus : il reste le temps de le lire.
                let resp = ui.interact(
                    r,
                    ui.make_persistent_id("toast_close"),
                    egui::Sense::click(),
                );
                hovered = resp.hovered();
                dismiss = resp.clicked();
            });
        if dismiss {
            self.toast = None;
        } else if hovered && let Some(t) = &mut self.toast {
            t.until = t.until.max(self.now + 1.5);
        }
    }
}

impl eframe::App for App {
    /// Appelé à chaque image, et aussi quand la fenêtre est réduite : les tâches de fond
    /// (libération mémoire du moteur, MIDI, LED, sons chargés) ne s'arrêtent jamais.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.now = ctx.input(|i| i.time);
            self.crash_safety_tick(ctx);
            // Panique rattrapée dans un callback audio : notée ici, hors du thread audio.
            if let Some(at) = audio::take_audio_panic() {
                crate::log_error(
                    "audio-panic",
                    &format!("Panique dans un callback audio ({at}) : flux coupé puis rouvert"),
                );
            }
            self.engine.collect_garbage();
            if self.cache.poll() {
                self.sb.midi_dirty = true;
            }
            self.update_playhead();
            self.flush_mix();
            self.complete_journal();
            self.poll_finishing(None);
            self.poll_save_as();
            if let Some(m) = crate::sessions::take_prefs_notice() {
                self.notify(m, true);
            }
            self.poll_play_errors();
            self.watch_device();
            self.watch_midi();
            self.poll_import();
            self.soundboard_background(ctx);
            self.update_background(ctx);
        }));
        if res.is_err() {
            self.notify("Erreur interne · l'audio continue", true);
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
            self.notify(format!("Erreur interne · l'audio continue : {msg}"), true);
        }
        // À part : le message d'erreur et la confirmation de fermeture doivent s'afficher
        // même si le reste de l'interface a planté. Chacun a son propre garde-fou : une
        // erreur ici ne doit pas non plus remonter jusqu'à eframe (fin du processus).
        let toast = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.toast_ui(&ctx)));
        if toast.is_err() {
            // Le message lui-même pose problème : on le retire (sinon erreur à chaque image).
            self.toast = None;
        }
        let dialog = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.confirm_close_dialog(&ctx)
        }));
        if dialog.is_err() {
            self.notify("Erreur interne · l'audio continue", true);
        }
        // 60 images/s quand quelque chose bouge (son, transport, animation), 10 sinon
        // (`logic` repasse toutes les 100 ms) : pas de calcul inutile à l'arrêt.
        // Pendant une prise sans interaction (des heures, souvent fenêtre au repos) : 30
        // images/s suffisent aux vumètres et à la tête de lecture, deux fois moins de calcul.
        if self.needs_animation() {
            let calm = self.is_recording()
                && ctx.input(|i| {
                    i.events.is_empty() && !i.pointer.is_moving() && !i.pointer.any_down()
                });
            ctx.request_repaint_after(std::time::Duration::from_millis(if calm { 33 } else { 16 }));
        }
    }

    fn on_exit(&mut self) {
        // Fermeture voulue : pas de relance automatique, même si eframe plante ensuite.
        crate::shutdown::mark_exit_requested();
        self.shutdown(ShutdownPace::Normal);
    }
}

impl Drop for App {
    /// Normalement déjà fait par `on_exit`. Sinon (erreur fatale du rendu, de winit ou
    /// d'eframe qui remonte jusqu'à `main`, fin anormale de la boucle d'événements),
    /// l'application est détruite pendant la remontée de l'erreur : c'est la dernière
    /// occasion d'arrêter la prise, de finaliser ses fichiers et de sauvegarder la session.
    /// Au plus vite dans ce cas : la relance automatique attend la fin de cette fermeture.
    fn drop(&mut self) {
        let pace = if crate::shutdown::exit_requested() {
            ShutdownPace::Normal
        } else {
            ShutdownPace::Hurry
        };
        self.shutdown(pace);
    }
}

/// Rythme de la fermeture propre.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShutdownPace {
    /// Fermeture voulue : les copies en cours (« Enregistrer sous », import) se terminent.
    Normal,
    /// Après une erreur fatale (relance en attente) ou à la fin de session Windows (le
    /// processus va être tué) : prises finalisées et session sauvegardée, le reste est
    /// abandonné comme à une fermeture brutale, avec des délais courts.
    Hurry,
}

/// Délais d'attente de la fermeture propre.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ShutdownWaits {
    /// Fin de la prise (intégration à la session) : abandon après le premier délai sans
    /// progrès ou le second en tout ; au-delà, récupération par le journal.
    finishing: (std::time::Duration, std::time::Duration),
    /// Sauvegarde automatique déjà lancée.
    autosave: std::time::Duration,
    /// Copies « Enregistrer sous » et import (`None` : abandonnées).
    copies: Option<std::time::Duration>,
    /// Copies de sécurité sur le second disque (sans progrès, en tout).
    backups: (std::time::Duration, std::time::Duration),
    /// Double de la session sur le second disque.
    backup_session: std::time::Duration,
}

impl ShutdownPace {
    fn waits(self) -> ShutdownWaits {
        use std::time::Duration;
        match self {
            Self::Normal => ShutdownWaits {
                finishing: (Duration::from_secs(15), Duration::from_secs(300)),
                autosave: Duration::from_secs(10),
                copies: Some(Duration::from_secs(120)),
                backups: (Duration::from_secs(15), Duration::from_secs(300)),
                backup_session: Duration::from_secs(10),
            },
            Self::Hurry => ShutdownWaits {
                finishing: (Duration::from_secs(3), Duration::from_secs(5)),
                autosave: Duration::from_secs(2),
                copies: None,
                backups: (Duration::from_secs(2), Duration::from_secs(2)),
                backup_session: Duration::from_secs(1),
            },
        }
    }
}

/// Message affiché après une relance automatique. `take_interrupted` : une prise était en
/// cours (son journal est resté) ; `opened` : message de l'ouverture de la session.
fn relaunch_message(take_interrupted: bool, opened: Option<&str>) -> String {
    let mut m = String::from("TracksPlus relancé après une erreur");
    m.push_str(if take_interrupted {
        " · prise arrêtée, gardée"
    } else {
        " · vérifiez la session"
    });
    if let Some(o) = opened.filter(|o| !o.is_empty()) {
        m.push_str(" · ");
        m.push_str(o);
    }
    m
}

/// Garde-fous du processus, à l'ouverture de la fenêtre : erreurs de la carte graphique
/// (qui feraient sinon planter toute l'application) et fin de session Windows.
fn install_crash_safety(cc: &eframe::CreationContext<'_>) {
    if let Some(rs) = &cc.wgpu_render_state {
        // Par défaut, wgpu plante sur toute erreur non rattrapée (mémoire graphique
        // épuisée, erreur de validation) : on la note et l'application continue.
        rs.device
            .on_uncaptured_error(Arc::new(|e: eframe::wgpu::Error| {
                // Filtre par genre d'erreur : une erreur répétée à chaque image (avec des
                // détails qui changent) n'écrit qu'une ligne par seconde.
                let key = match &e {
                    eframe::wgpu::Error::OutOfMemory { .. } => "wgpu-out-of-memory",
                    eframe::wgpu::Error::Validation { .. } => "wgpu-validation",
                    eframe::wgpu::Error::Internal { .. } => "wgpu-internal",
                };
                crate::log_error(key, &format!("Erreur de la carte graphique : {e}"));
            }));
        // Pilote réinitialisé : egui ne recrée pas le contexte graphique, l'affichage ne
        // reviendra pas. La prise, elle, continue.
        rs.device.set_device_lost_callback(|reason, msg| {
            if reason != eframe::wgpu::DeviceLostReason::Destroyed {
                crate::shutdown::set_gpu_lost();
                crate::log_error(
                    "wgpu-device-lost",
                    &format!("Carte graphique perdue ({reason:?}) : {msg}"),
                );
            }
        });
    }
    #[cfg(windows)]
    {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        if let Ok(h) = cc.window_handle()
            && let RawWindowHandle::Win32(w) = h.as_raw()
        {
            crate::shutdown::install(w.hwnd.get());
        }
    }
}

impl App {
    /// Toutes les 100 ms (`logic`) : état partagé avec la fin de session Windows et la fin
    /// du processus, messages des incidents survenus hors de l'interface.
    fn crash_safety_tick(&mut self, ctx: &egui::Context) {
        let busy = self.take_in_progress();
        crate::shutdown::set_take_in_progress(busy);
        crate::shutdown::sync_block_reason(busy);
        // Fermeture acceptée (pas de confirmation à demander) : une erreur pendant la
        // fermeture ne doit pas relancer l'application.
        if !busy && self.export_job.is_none() && ctx.input(|i| i.viewport().close_requested()) {
            crate::shutdown::mark_exit_requested();
        }
        if crate::shutdown::session_ended() {
            // Windows ferme la session : les prises ont été finalisées en urgence. Si le
            // processus n'est pas encore tué, on termine proprement (prises réintégrées,
            // session sauvegardée) et on ferme, au plus vite.
            crate::shutdown::mark_exit_requested();
            self.shutdown(ShutdownPace::Hurry);
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        if crate::shutdown::take_save_request() {
            // Arrêt de Windows annoncé : session et soundboard sauvegardés tout de suite
            // (la prise en cours, elle, est protégée par son journal).
            self.autosave();
        }
        self.sync_pending_saves();
        if crate::shutdown::take_shutdown_refused() {
            self.notify("Arrêt de Windows bloqué · la prise continue", true);
        }
        if crate::shutdown::take_gpu_lost() {
            self.notify(
                "Affichage perdu · la prise continue · arrêt au clavier",
                true,
            );
        }
    }

    /// Sauvegardes prêtes pour une fin de session Windows qui arriverait avant le prochain
    /// passage de l'interface (voir `shutdown::set_pending_session_save`) : préparées tant
    /// qu'il y a des modifications non sauvegardées (état figé au plus une fois par
    /// seconde), retirées sinon.
    fn sync_pending_saves(&mut self) {
        let session = self.project.dirty && self.worth_saving();
        let board = self.board_changed.is_some();
        if !session && !board {
            if self.pending_saves_at.is_some() {
                self.pending_saves_at = None;
                crate::shutdown::set_pending_session_save(None);
                crate::shutdown::set_pending_board_save(None);
            }
            return;
        }
        if self
            .pending_saves_at
            .is_some_and(|t| self.now - t < PENDING_SAVES_EVERY)
        {
            return;
        }
        self.pending_saves_at = Some(self.now);
        crate::shutdown::set_pending_session_save(session.then(|| {
            // Rang de sauvegarde pris maintenant : n'écrase jamais une sauvegarde plus
            // récente, faite entre-temps par l'interface.
            let save = self.project.deferred_save();
            Box::new(move || {
                let _ = save();
            }) as crate::shutdown::PendingWrite
        }));
        crate::shutdown::set_pending_board_save(board.then(|| {
            let b = self.board.clone();
            let dir = self.project.dir.clone();
            let session_file = self.project.file_path();
            Box::new(move || {
                let _ = b.save_devices();
                // (Comme `save_board_now` : les pads vont dans une session déjà sur le disque.)
                if session_file.exists() {
                    let _ = b.save_session(&dir);
                }
            }) as crate::shutdown::PendingWrite
        }));
    }

    /// Fermeture propre, une seule fois (`on_exit`, ou destruction de l'application après
    /// une erreur fatale). Protégée : si elle échoue en route, les prises encore en
    /// écriture sont au moins finalisées (le journal permet de les réintégrer ensuite).
    fn shutdown(&mut self, pace: ShutdownPace) {
        if !crate::shutdown::begin_shutdown() {
            return;
        }
        let waits = pace.waits();
        let res =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.shutdown_steps(waits)));
        if res.is_err() {
            crate::recorder::emergency_finish(
                std::time::Duration::from_secs(3),
                std::time::Duration::from_secs(60),
            );
            crate::recorder::wait_backups(waits.backups.0, waits.backups.1);
        }
    }

    fn shutdown_steps(&mut self, waits: ShutdownWaits) {
        self.midi.leds_off(self.board.led_mode);
        self.stop();
        self.finish_export_for_exit();
        // Import en cours : annulé (les fichiers à moitié convertis sont supprimés).
        if let Some(job) = self.audio_import.take() {
            job.cancel.store(true, Ordering::Relaxed);
            let _ = job.join();
        }
        self.poll_finishing(Some(waits.finishing));
        self.settle_autosave_within(waits.autosave);
        // Copie en cours (« Enregistrer sous », import) : on la laisse se terminer, sauf au
        // plus vite (abandonnée, comme à une fermeture brutale).
        let copies = [
            self.save_as_job.take().map(|r| {
                Box::new(move |t| r.recv_timeout(t).is_ok())
                    as Box<dyn FnOnce(std::time::Duration) -> bool>
            }),
            self.import_job.take().map(|r| {
                Box::new(move |t| r.recv_timeout(t).is_ok())
                    as Box<dyn FnOnce(std::time::Duration) -> bool>
            }),
        ];
        if let Some(t) = waits.copies {
            for rx in copies.into_iter().flatten() {
                let _ = rx(t);
            }
        }
        self.autosave();
        self.board_changed = Some(0.0);
        self.save_board_now();
        // Copie de sécurité : fichiers des prises et double de la session finis d'écrire
        // sur le second disque, tant que l'écriture avance (un disque mort ne bloque pas
        // la fermeture : abandon après un moment sans aucun progrès).
        crate::recorder::wait_backups(waits.backups.0, waits.backups.1);
        self.backup_jobs = None;
        if let Some(h) = self.backup_thread.take() {
            let t0 = std::time::Instant::now();
            while !h.is_finished() && t0.elapsed() < waits.backup_session {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }
}

/// Fréquence maximale de préparation des sauvegardes de fin de session (`sync_pending_saves`).
const PENDING_SAVES_EVERY: f64 = 1.0;

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
        // Sauvegarde automatique des modifications (montage, pistes, repères…), aussi
        // pendant une prise : en arrière-plan, et la session écrite ne connaît pas encore la
        // prise en cours (le journal de secours s'en charge), donc rien à craindre.
        if self.now - self.last_autosave > 30.0 && self.project.dirty && self.worth_saving() {
            self.last_autosave = self.now;
            if self.autosave_job.is_none() {
                let rx = self.project.save_in_background();
                self.autosave_job = Some((self.project.dir.clone(), rx));
            }
            // Double de la session sur le second disque tenu à jour lui aussi (au plus une
            // fois par minute pendant une prise).
            if self.is_recording() && self.now - self.backup_double_at > 60.0 {
                self.backup_double_at = self.now;
                let j = self.rec_journal.clone().filter(|_| self.rec_has_backup);
                self.backup_session_job(j, false);
            }
        }
        if let Some((dir, rx)) = &self.autosave_job {
            let same = *dir == self.project.dir;
            match rx.try_recv() {
                Ok(r) => {
                    self.autosave_job = None;
                    // (Une session quittée entre-temps a été réglée par `settle_autosave`.)
                    if let Err(e) = r
                        && same
                    {
                        self.save_failed(e);
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.autosave_job = None;
                    if same {
                        self.project.dirty = true;
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
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
        self.incidents_dialog(&ctx);
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
    none_item: Option<&str>,
    none_text: &str,
) -> bool {
    let mut unprobed = false;
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
            if let Some(none_item) = none_item
                && ui.selectable_label(device.is_none(), none_item).clicked()
            {
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
                        // Pilote pas encore interrogé : il l'est tout de suite.
                        unprobed |= d.note.is_some();
                    }
                }
            }
        });
    unprobed
}

/// Périphériques dans l'ordre naturel : chaque famille (« Analog », « ADAT »…) dans l'ordre
/// où Windows la donne, et dans une famille, les numéros dans l'ordre (« (1+2) » avant
/// « (7+8) », « 10 » après « 9 ») : Windows les mélange.
fn natural_order(mut v: Vec<DeviceInfo>) -> Vec<DeviceInfo> {
    fn family(s: &str) -> String {
        s.chars()
            .take_while(|c| !c.is_ascii_digit())
            .flat_map(char::to_lowercase)
            .collect()
    }
    fn numbers(s: &str) -> Vec<u64> {
        s.split(|c: char| !c.is_ascii_digit())
            .filter(|n| !n.is_empty())
            .map(|n| n.parse().unwrap_or(u64::MAX))
            .collect()
    }
    let mut families: Vec<String> = Vec::new();
    for d in &v {
        let f = family(&d.name);
        if !families.contains(&f) {
            families.push(f);
        }
    }
    v.sort_by_cached_key(|d| {
        let f = family(&d.name);
        let rank = families.iter().position(|x| *x == f).unwrap_or(usize::MAX);
        (rank, numbers(&d.name), d.name.to_lowercase())
    });
    v
}

/// Carte son d'un périphérique. ASIO : le pilote lui-même. Ailleurs (WASAPI…) : le nom
/// entre parenthèses à la fin, comme Windows nomme ses périphériques (« DVS Receive 1-2
/// (Dante Virtual Soundcard) », « Haut-parleurs (Realtek(R) Audio) »), sinon le nom.
fn card_of(device: &str, asio: bool) -> String {
    let name = device.trim();
    if asio || !name.ends_with(')') {
        return name.to_string();
    }
    let mut depth = 0usize;
    for (i, ch) in name.char_indices().rev() {
        match ch {
            ')' => depth += 1,
            '(' => {
                depth -= 1;
                if depth == 0 {
                    let inner = name[i + 1..name.len() - 1].trim();
                    return if inner.is_empty() || i == 0 {
                        name.to_string()
                    } else {
                        inner.to_string()
                    };
                }
            }
            _ => {}
        }
    }
    name.to_string()
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
mod take_tests {
    use super::*;

    fn res(track_id: u64, segs: &[(&str, u64, u64)]) -> RecResult {
        RecResult {
            track_id,
            segments: segs
                .iter()
                .map(|&(f, start, frames)| Segment {
                    file: f.into(),
                    frames,
                    start,
                })
                .collect(),
            error: None,
        }
    }

    /// Placement d'une prise : latence sautée au début, trou conservé après une erreur,
    /// fichier vide écarté (supprimé seulement après la sauvegarde).
    #[test]
    fn layout_places_segments_and_discards_empty_ones() {
        let r = [res(
            7,
            &[
                ("V.wav", 0, 1000),
                ("V_p2.wav", 1000, 0),
                ("V_p3.wav", 1500, 500),
            ],
        )];
        let mut next = 100;
        let l = layout_take(&r, 10_000, 40, &mut || {
            next += 1;
            next
        });
        let got: Vec<_> = l
            .clips
            .iter()
            .map(|(t, c, _)| (*t, c.id, c.file.as_str(), c.start, c.offset, c.len))
            .collect();
        assert_eq!(
            got,
            [
                (7, 101, "V.wav", 10_000, 40, 960),
                (7, 102, "V_p3.wav", 11_460, 0, 500)
            ]
        );
        assert_eq!(l.discard, ["V_p2.wav"]);
    }

    /// Copie de sécurité découpée autrement (trou sur un seul disque) : seule la piste
    /// concernée est signalée, et le double de la session référence les fichiers de la
    /// copie, déplacés comme les prises du principal l'ont été depuis.
    #[test]
    fn backup_alt_replaces_only_divergent_tracks() {
        let mut p = Project::new("S".into(), PathBuf::from("x"), 48000);
        let ta = p.add_track(Some("A".into()), Some(0));
        let tb = p.add_track(Some("B".into()), Some(1));
        let main = [
            res(ta, &[("A.wav", 0, 600), ("A_p2.wav", 900, 300)]),
            res(tb, &[("B.wav", 0, 1200)]),
        ];
        let copy = [
            res(ta, &[("A.wav", 0, 1200)]),
            res(tb, &[("B.wav", 0, 1200)]),
        ];
        let m = layout_take(&main, 0, 0, &mut || p.alloc_id());
        let b = layout_take(&copy, 0, 0, &mut || p.alloc_id());
        let m: Vec<(u64, Clip)> = m.clips.into_iter().map(|(t, c, _)| (t, c)).collect();
        let b: Vec<(u64, Clip)> = b.clips.into_iter().map(|(t, c, _)| (t, c)).collect();
        let div = backup_divergence(&m, &b);
        assert_eq!(div.len(), 1);
        assert_eq!(div[0].0, ta);
        for (t, c) in &m {
            p.track_mut(*t).unwrap().clips.push(c.clone());
        }
        // Les deux prises de la piste A ont été déplacées ensemble de 100 trames.
        for c in &mut p.track_mut(ta).unwrap().clips {
            c.start += 100;
        }
        let alts: Vec<BackupAlt> = div
            .into_iter()
            .map(|(track_id, main, backup)| BackupAlt {
                dir: PathBuf::from("x"),
                track_id,
                main,
                backup,
            })
            .collect();
        let shape = |p: &Project, t: u64| -> Vec<(String, u64, u64)> {
            p.tracks
                .iter()
                .find(|x| x.id == t)
                .unwrap()
                .clips
                .iter()
                .map(|c| (c.file.clone(), c.start, c.len))
                .collect()
        };
        let mut q = p.clone();
        apply_backup_alts(&mut q, &alts);
        assert_eq!(shape(&q, ta), [("A.wav".to_string(), 100, 1200)]);
        assert_eq!(shape(&q, tb), shape(&p, tb), "piste identique intacte");
        // Prise retouchée depuis (raccourcie) : la copie ne peut plus la suivre.
        let mut r = p.clone();
        r.track_mut(ta).unwrap().clips[0].len -= 10;
        let before = shape(&r, ta);
        apply_backup_alts(&mut r, &alts);
        assert_eq!(shape(&r, ta), before);
    }

    #[test]
    fn join_few_counts_the_rest() {
        let v: Vec<String> = (0..5).map(|i| format!("e{i}")).collect();
        assert_eq!(join_few(&v, 3), "e0 ; e1 ; e2 (+2 autre(s))");
        assert_eq!(join_few(&v[..2], 3), "e0 ; e1");
    }

    use std::sync::atomic::AtomicU64;

    fn take_end(dir: &str, files: &[&str]) -> TakeEnd {
        TakeEnd {
            released: true,
            live: Vec::new(),
            lat: 0,
            lost_card: false,
            split: false,
            gap: 0,
            gap_missing: 0,
            backup_warn: Vec::new(),
            overruns: 0,
            xruns: 0,
            merge: false,
            dir: PathBuf::from(dir),
            journal: Some(recovery::Journal {
                start: 10,
                play_offset: 0,
                latency: 0,
                sample_rate: 48000,
                takes: files
                    .iter()
                    .map(|f| recovery::JournalTake {
                        track_id: 1,
                        file: (*f).into(),
                    })
                    .collect(),
                // Une prise bloquée plus ancienne portée par celle-ci : jamais dupliquée.
                extra: vec![recovery::Journal {
                    start: 0,
                    play_offset: 0,
                    latency: 0,
                    sample_rate: 48000,
                    takes: Vec::new(),
                    extra: Vec::new(),
                }],
            }),
        }
    }

    /// Nouvelle prise pendant la fin d'écriture de la précédente : attente tant que le
    /// disque avance, départ quand il est bloqué, refus au-delà de quelques prises bloquées.
    #[test]
    fn record_over_finishing_take() {
        assert_eq!(over_finishing(false, false, 0), OverFinishing::Free);
        assert_eq!(over_finishing(false, false, 5), OverFinishing::Free);
        assert_eq!(over_finishing(true, false, 0), OverFinishing::Wait);
        assert_eq!(over_finishing(true, true, 0), OverFinishing::Adopt);
        assert_eq!(
            over_finishing(true, true, MAX_STALLED_TAKES - 1),
            OverFinishing::Adopt
        );
        assert_eq!(
            over_finishing(true, true, MAX_STALLED_TAKES),
            OverFinishing::TooMany
        );
    }

    /// Journal réécrit après l'intégration d'une prise : seulement les prises de la session
    /// encore en cours d'écriture, sans les prises qu'elles portaient ; rien → supprimé.
    #[test]
    fn journal_keeps_only_inflight_takes_of_session() {
        let takes = [
            take_end("S", &["A_001.wav"]),
            take_end("Autre", &["B_001.wav"]),
            take_end("S", &["A_002.wav", "C_001.wav"]),
        ];
        let j = journals_of(takes.iter(), Path::new("S"));
        assert_eq!(j.len(), 2);
        assert!(j.iter().all(|j| j.extra.is_empty()));
        assert_eq!(j[1].takes.len(), 2);
        let r = remaining_journal(j, 48000).unwrap();
        assert!(r.takes.is_empty());
        assert_eq!(r.extra.len(), 2);
        assert_eq!(r.extra[0].takes[0].file, "A_001.wav");
        assert!(remaining_journal(Vec::new(), 48000).is_none());
        assert!(journals_of(takes.iter(), Path::new("Rien")).is_empty());
    }

    /// Prise terminée dont le fichier est déjà dans la session : prolongée jusqu'au bout
    /// du fichier (sans doublon) ; une prise raccourcie à la main garde sa longueur ; un
    /// fichier absent de la session est à ajouter.
    #[test]
    fn finished_take_extends_existing_clip() {
        let mut p = Project::new("E".into(), PathBuf::from("e"), 48000);
        let t = p.add_track(Some("Voix".into()), Some(0));
        let clip = |id, file: &str, len, src_len| Clip {
            id,
            file: file.into(),
            start: 100,
            offset: 0,
            len,
            src_len,
            fade_in: 0,
            fade_out: 0,
        };
        let tr = p.track_mut(t).unwrap();
        tr.clips.push(clip(1, "Voix_001.wav", 1000, 1000));
        tr.clips.push(clip(2, "Voix_002.wav", 300, 1000));
        assert_eq!(extend_existing(&mut p, "Voix_001.wav", 5000), [1]);
        assert_eq!(extend_existing(&mut p, "Voix_002.wav", 5000), [2]);
        assert!(extend_existing(&mut p, "Voix_003.wav", 5000).is_empty());
        let c: Vec<_> = p.tracks[0]
            .clips
            .iter()
            .map(|c| (c.id, c.len, c.src_len))
            .collect();
        assert_eq!(c, [(1, 5000, 5000), (2, 300, 5000)]);
        // Fichier finalement plus court que la prise réintégrée : recalée.
        assert_eq!(extend_existing(&mut p, "Voix_001.wav", 800), [1]);
        assert_eq!(p.tracks[0].clips[0].len, 800);
    }

    fn finishing(rx: std::sync::mpsc::Receiver<Finished>, progress: Arc<AtomicU64>) -> Finishing {
        Finishing {
            rx,
            start: 0,
            take: take_end("S", &["A_001.wav"]),
            progress,
            seen: (0, 0.0),
            slow_told: false,
        }
    }

    /// Fermeture de l'application pendant une fin de prise : attente tant que l'écriture
    /// avance, abandon (journal gardé) seulement quand elle n'avance plus ; résultat
    /// tardif jamais jeté ; thread disparu → récupération depuis le disque.
    #[test]
    fn wait_finish_follows_progress() {
        use std::time::Duration;
        let idle = Duration::from_millis(150);
        let cap = Duration::from_secs(30);
        // Disque bloqué : aucun progrès.
        let (_tx, rx) = std::sync::mpsc::channel();
        let f = finishing(rx, Arc::new(AtomicU64::new(0)));
        assert!(matches!(
            App::wait_finish_for(&f, idle, cap),
            TakeOutcome::StillWriting
        ));
        // Disque lent qui avance : le résultat arrive bien après `idle`.
        let (tx, rx) = std::sync::mpsc::channel();
        let progress = Arc::new(AtomicU64::new(0));
        let f = finishing(rx, progress.clone());
        let h = std::thread::spawn(move || {
            for _ in 0..12 {
                std::thread::sleep(Duration::from_millis(50));
                progress.fetch_add(1, Ordering::Relaxed);
            }
            let _ = tx.send(Finished {
                main: Ok(Vec::new()),
                backup: None,
            });
        });
        assert!(matches!(
            App::wait_finish_for(&f, idle, cap),
            TakeOutcome::Done(Finished { main: Ok(_), .. })
        ));
        h.join().unwrap();
        // Thread de fin disparu sans résultat.
        let (tx, rx) = std::sync::mpsc::channel::<Finished>();
        drop(tx);
        let f = finishing(rx, Arc::new(AtomicU64::new(0)));
        assert!(matches!(
            App::wait_finish_for(&f, idle, cap),
            TakeOutcome::Lost
        ));
    }

    /// Préparation du dossier d'une prise hors du thread de l'interface : dossier créé,
    /// journal existant lu, noms présents (minuscules).
    #[test]
    fn probe_audio_dir_prepares_take_folder() {
        let dir = std::env::temp_dir().join(format!("tp-probe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let audio = dir.join(project::AUDIO_DIR);
        let journal = recovery::journal_path(&dir);
        let timeout = std::time::Duration::from_secs(10);
        let (j, names) = probe_audio_dir(audio.clone(), journal.clone(), timeout)
            .unwrap()
            .unwrap();
        assert!(audio.is_dir());
        assert!(j.is_none());
        assert!(names.is_empty());
        std::fs::write(audio.join("Voix_001.WAV"), b"x").unwrap();
        std::fs::write(&journal, "{}").unwrap();
        let (j, names) = probe_audio_dir(audio, journal, timeout).unwrap().unwrap();
        assert_eq!(j.as_deref(), Some("{}"));
        assert!(names.contains("voix_001.wav"));
        std::fs::remove_dir_all(dir).ok();
    }
}

#[cfg(test)]
mod tests {
    /// Prise relancée après une perte de la carte : posée à l'heure réelle (fin de ce qui a
    /// été capté + durée du trou), jamais avant.
    #[test]
    fn resumed_take_lands_after_the_gap() {
        assert_eq!(
            super::resume_position(480_000, 4.5, 48000),
            480_000 + 216_000
        );
        assert_eq!(super::resume_position(1000, -3.0, 48000), 1000);
        assert_eq!(super::fmt_gap(4.25), "4,2 s");
        assert_eq!(super::fmt_gap(125.0), "2 min 05");
    }

    /// Une erreur affichée n'est jamais effacée par le message suivant : il s'ajoute, en
    /// entier, et la liste reste d'une longueur lisible. Partent d'abord les informations,
    /// puis les états de la carte (même composés de « · »), jamais une prise interrompue ni
    /// le message le plus récent.
    #[test]
    fn toast_messages_pile_up_without_growing_forever() {
        use super::{Level, push_toast};
        let mut v = Vec::new();
        push_toast(
            &mut v,
            Level::Critical,
            "Carte son perdue pendant la prise : enregistrement arrêté".into(),
        );
        push_toast(&mut v, Level::Info, "Carte son reconnectée".into());
        push_toast(&mut v, Level::Info, "Carte son reconnectée".into());
        assert_eq!(v.len(), 2);
        // Notices de carte longues, faites de morceaux séparés par « · ».
        let notice = |k: usize| format!("Carte son : Entrée « A{k} » perdue · {}", "x".repeat(250));
        for k in 0..6 {
            push_toast(&mut v, Level::Device, notice(k));
        }
        push_toast(
            &mut v,
            Level::Warn,
            "Prise enregistrée, mais 3 saturations disque".into(),
        );
        let len: usize = v.iter().map(|(_, t)| t.chars().count() + 1).sum();
        assert!(len <= 900, "{len}");
        // La prise interrompue est toujours là, en entier ; l'information est partie avant
        // les notices ; la plus récente notice restante est entière (jamais coupée).
        assert_eq!(v[0].0, Level::Critical);
        assert!(v.iter().all(|(l, _)| *l != Level::Info));
        assert!(v.iter().any(|(_, t)| *t == notice(5)));
        assert_eq!(v.last().unwrap().0, Level::Warn);
        // Un seul message énorme : gardé entier.
        let mut w = vec![(Level::Info, "ancien".to_string())];
        let huge = "y".repeat(2000);
        push_toast(&mut w, Level::Warn, huge.clone());
        assert_eq!(w, vec![(Level::Warn, huge)]);
        // Uniquement des prises interrompues : rien n'est retiré.
        let mut c = Vec::new();
        for k in 0..5 {
            push_toast(&mut c, Level::Critical, format!("{k}{}", "z".repeat(300)));
        }
        assert_eq!(c.len(), 5);
    }

    /// Relance automatique : annulée si l'opérateur reprend la main, abandonnée au-delà
    /// de `RESUME_MAX` (carte rallumée le lendemain : aucune prise ne part seule), lancée
    /// seulement quand l'entrée revenue a vraiment battu.
    #[test]
    fn resume_waits_for_the_input_and_expires() {
        use super::{RESUME_MAX, ResumeCtx, ResumeStep, resume_step};
        let ok = ResumeCtx {
            age: 5.0,
            same_session: true,
            enabled: true,
            disk_stalled: false,
            transport_stopped: true,
            idle: true,
            input_ready: true,
            beat: 100,
            hb_seen: None,
        };
        assert_eq!(resume_step(&ok), ResumeStep::Arm(100));
        assert_eq!(
            resume_step(&ResumeCtx {
                hb_seen: Some(100),
                ..ok
            }),
            ResumeStep::Wait
        );
        assert_eq!(
            resume_step(&ResumeCtx {
                hb_seen: Some(99),
                ..ok
            }),
            ResumeStep::Go
        );
        // Entrée absente ou prise précédente en finalisation : on attend.
        for c in [
            ResumeCtx {
                input_ready: false,
                ..ok
            },
            ResumeCtx { idle: false, ..ok },
        ] {
            assert_eq!(resume_step(&c), ResumeStep::Wait);
        }
        // Trop tard, même si l'entrée est revenue.
        assert_eq!(
            resume_step(&ResumeCtx {
                age: RESUME_MAX + 1.0,
                hb_seen: Some(1),
                ..ok
            }),
            ResumeStep::Expire
        );
        assert_eq!(
            resume_step(&ResumeCtx {
                age: RESUME_MAX + 1.0,
                input_ready: false,
                ..ok
            }),
            ResumeStep::Expire
        );
        // L'opérateur a repris la main.
        for c in [
            ResumeCtx {
                same_session: false,
                ..ok
            },
            ResumeCtx {
                enabled: false,
                ..ok
            },
            ResumeCtx {
                transport_stopped: false,
                ..ok
            },
        ] {
            assert_eq!(resume_step(&c), ResumeStep::Cancel);
        }
        // Disque qui ne répond plus : abandon signalé (pas une annulation silencieuse).
        let blocked = ResumeCtx {
            disk_stalled: true,
            ..ok
        };
        assert_eq!(resume_step(&blocked), ResumeStep::Blocked);
    }

    /// Chien de garde : flux muet signalé après `STREAM_STALL` (l'entrée en prise après
    /// `REC_STALL_STOP`), rien jugé sur un saut de temps ni sur un flux fermé, flux qui
    /// bat de nouveau rendu dans `beating`.
    #[test]
    fn watchdog_flags_silent_streams_only() {
        use super::{REC_STALL_STOP, STREAM_STALL, watchdog};
        use crate::audio::{LOST_INPUT, LOST_MONITOR, LOST_OUTPUT, SLOTS};
        let mut hb = [(0u64, 0.0f64); SLOTS];
        let open = LOST_INPUT | LOST_OUTPUT;
        // Premier passage : tout bat (référence posée).
        let (s, b) = watchdog(&mut hb, [1, 1, 1, 1], open, 0.0, false, false);
        assert_eq!((s, b), (0, open));
        // Muets depuis peu : rien.
        let (s, b) = watchdog(
            &mut hb,
            [1, 1, 1, 1],
            open,
            STREAM_STALL - 0.5,
            false,
            false,
        );
        assert_eq!((s, b), (0, 0));
        // Saut dans le temps : rien jugé, références repartent de là.
        let t = STREAM_STALL + 10.0;
        assert_eq!(watchdog(&mut hb, [1, 1, 1, 1], open, t, true, false).0, 0);
        assert_eq!(
            watchdog(
                &mut hb,
                [1, 1, 1, 1],
                open,
                t + STREAM_STALL - 0.5,
                false,
                false
            )
            .0,
            0
        );
        // Muets trop longtemps : signalés (seulement les flux ouverts).
        let t2 = t + STREAM_STALL + 0.5;
        let (s, _) = watchdog(&mut hb, [1, 1, 1, 1], open, t2, false, false);
        assert_eq!(s, open);
        assert_eq!(s & LOST_MONITOR, 0);
        // Pendant une prise, l'entrée a droit à plus.
        let (s, _) = watchdog(&mut hb, [1, 1, 1, 1], open, t2, false, true);
        assert_eq!(s, LOST_OUTPUT);
        // La sortie repart : rendue dans `beating`, plus signalée.
        let (s, b) = watchdog(&mut hb, [1, 2, 1, 1], open, t2 + 0.1, false, true);
        assert_eq!((s & LOST_OUTPUT, b), (0, LOST_OUTPUT));
        let (s, _) = watchdog(
            &mut hb,
            [1, 2, 1, 1],
            open,
            t + REC_STALL_STOP + 1.0,
            false,
            true,
        );
        assert_eq!(s, LOST_INPUT | LOST_OUTPUT);
        // Flux fermés : jamais signalés.
        let (s, b) = watchdog(&mut hb, [1, 2, 1, 1], 0, 1e6, false, false);
        assert_eq!((s, b), (0, 0));
    }

    /// Choix des flux à rouvrir (hors ASIO) : jamais l'entrée pendant une prise ; sortie
    /// soundboard revenue avec les pads repliés sur le main : main rouvert avec elle,
    /// seulement à l'arrêt, sans pad en lecture et si elle ne refuse pas de s'ouvrir.
    #[test]
    fn reopen_slots_respect_take_and_pads() {
        use super::{ReopenCtx, reopen_slots};
        use crate::audio::expand_slots;
        use crate::audio::{LOST_ALL, LOST_INPUT, LOST_MONITOR, LOST_OUTPUT, LOST_PADS};
        let idle = ReopenCtx {
            recording: false,
            stopped: true,
            pads_playing: false,
            refused: 0,
        };
        let folded = |s| expand_slots(s, LOST_ALL, true, true);
        let separate = |s| expand_slots(s, LOST_ALL, false, true);
        assert_eq!(
            reopen_slots(LOST_INPUT | LOST_MONITOR, &idle, folded),
            LOST_INPUT | LOST_MONITOR
        );
        let rec = ReopenCtx {
            recording: true,
            stopped: false,
            ..idle
        };
        assert_eq!(
            reopen_slots(LOST_INPUT | LOST_MONITOR, &rec, folded),
            LOST_MONITOR
        );
        // Sortie soundboard de retour, pads joués sur le main (« gated »).
        assert_eq!(reopen_slots(LOST_PADS, &idle, folded), LOST_PADS);
        assert_eq!(
            folded(LOST_PADS),
            (LOST_PADS | LOST_OUTPUT, LOST_PADS, LOST_PADS | LOST_OUTPUT)
        );
        for c in [
            ReopenCtx {
                stopped: false,
                ..idle
            },
            ReopenCtx {
                pads_playing: true,
                ..idle
            },
            ReopenCtx {
                refused: LOST_PADS,
                ..idle
            },
        ] {
            assert_eq!(reopen_slots(LOST_PADS, &c, folded), 0);
        }
        // Pads sur leur propre sortie : rien à garder, pas de main touché.
        assert_eq!(separate(LOST_PADS), (LOST_PADS, 0, 0));
        assert_eq!(
            reopen_slots(
                LOST_PADS,
                &ReopenCtx {
                    pads_playing: true,
                    ..idle
                },
                separate
            ),
            LOST_PADS
        );
        // Main fermé (perdu lui aussi) : rouvert avec, sans condition de présence.
        assert_eq!(
            expand_slots(LOST_PADS | LOST_OUTPUT, LOST_ALL, true, true).2,
            0
        );
        assert_eq!(
            expand_slots(LOST_PADS, LOST_ALL, true, false),
            (LOST_PADS, 0, 0)
        );
        // Flux non attendus (pas de sortie soundboard à part) : jamais rouverts.
        assert_eq!(
            expand_slots(LOST_PADS, LOST_INPUT | LOST_OUTPUT, true, true).0,
            0
        );
    }

    /// Copie de sécurité : sons du soundboard copiés, pads pointant vers la copie (qui
    /// marche donc sans le disque principal) ; un son hors de la session garde son chemin.
    #[test]
    fn backup_board_copies_sounds() {
        use crate::soundboard::{Board, Pad, SOUNDS_DIR};
        let base = std::env::temp_dir().join(format!("tp-bboard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (from, to) = (base.join("main/S"), base.join("copie/S (1)"));
        std::fs::create_dir_all(from.join(SOUNDS_DIR)).unwrap();
        std::fs::create_dir_all(&to).unwrap();
        std::fs::write(from.join(SOUNDS_DIR).join("a.wav"), b"son").unwrap();
        let mut b = Board::default();
        b.set(0, Some(Pad::new(from.join(SOUNDS_DIR).join("a.wav"), 0)));
        b.set(1, Some(Pad::new(base.join("ailleurs.wav"), 1)));
        super::backup_board(b, &from, &to);
        assert_eq!(
            std::fs::read(to.join(SOUNDS_DIR).join("a.wav")).unwrap(),
            b"son"
        );
        let back = Board::load_session(&to).unwrap();
        assert_eq!(back.pad(0).unwrap().path, to.join(SOUNDS_DIR).join("a.wav"));
        assert_eq!(back.pad(1).unwrap().path, base.join("ailleurs.wav"));
        std::fs::remove_dir_all(base).ok();
    }

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
            assert!(super::same_volume(
                Path::new(r"\\?\C:\Musique"),
                Path::new(r"c:\Secours")
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

    #[test]
    fn relaunch_message_keeps_the_session_message() {
        // Le message de l'ouverture de la session n'efface pas l'alerte : les deux restent.
        let m = relaunch_message(
            true,
            Some("Enregistrement interrompu récupéré : 3 prise(s)"),
        );
        assert!(m.starts_with("TracksPlus relancé après une erreur"));
        assert!(m.contains("prise arrêtée"));
        assert!(m.ends_with(" · Enregistrement interrompu récupéré : 3 prise(s)"));
        let m = relaunch_message(false, None);
        assert!(m.contains("vérifiez la session"));
        assert!(!m.ends_with(" · "));
        assert_eq!(relaunch_message(false, Some("")), m);
    }

    /// Cartes son : en WASAPI, les périphériques de Windows regroupés par carte (nom entre
    /// parenthèses, même imbriquées) ; en ASIO, chaque pilote est une carte.
    #[test]
    fn card_devices_are_in_natural_order() {
        let d = |n: &str| DeviceInfo {
            name: n.into(),
            channels: 2,
            rates: Vec::new(),
            default_rate: None,
            buffer: None,
            note: None,
        };
        let v = natural_order(vec![
            d("Analog (7+8) (RME Fireface UC)"),
            d("Analog (10+11) (RME Fireface UC)"),
            d("Analog (1+2) (RME Fireface UC)"),
            d("ADAT (1+2) (RME Fireface UC)"),
        ]);
        let names: Vec<&str> = v.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Analog (1+2) (RME Fireface UC)",
                "Analog (7+8) (RME Fireface UC)",
                "Analog (10+11) (RME Fireface UC)",
                "ADAT (1+2) (RME Fireface UC)"
            ]
        );
    }

    #[test]
    fn devices_are_grouped_by_card() {
        assert_eq!(
            card_of("DVS Receive  1-2 (Dante Virtual Soundcard)", false),
            "Dante Virtual Soundcard"
        );
        assert_eq!(
            card_of("Haut-parleurs (Realtek(R) Audio)", false),
            "Realtek(R) Audio"
        );
        assert_eq!(card_of("Micro USB", false), "Micro USB");
        assert_eq!(card_of("(étrange)", false), "(étrange)");
        assert_eq!(
            card_of("Dante Virtual Soundcard (x64)", true),
            "Dante Virtual Soundcard (x64)"
        );
    }

    #[test]
    fn hurried_shutdown_abandons_copies_and_waits_less() {
        let (n, h) = (ShutdownPace::Normal.waits(), ShutdownPace::Hurry.waits());
        assert!(n.copies.is_some() && h.copies.is_none());
        assert!(h.finishing < n.finishing && h.autosave < n.autosave);
        assert!(h.backups < n.backups && h.backup_session < n.backup_session);
        // Au plus vite : la relance n'attend pas plus de quelques secondes.
        let total = h.finishing.1 + h.autosave + h.backups.1 + h.backup_session;
        assert!(total <= std::time::Duration::from_secs(10));
    }
}
