//! Modèle du soundboard (pages de pads), sauvegarde, et chargement des sons
//! (WAV, AIFF, MP3, AAC/M4A, FLAC, OGG…) rééchantillonnés à la fréquence de la carte.

use crate::project::{
    Extra, FORMAT_VERSION, lenient, lenient_f32, lenient_f64, parse_json, write_format_version,
};
use crate::sampler::{MAX_PAGES, PADS_PER_PAGE, PadMode, PadParams, SAMPLE_PEAK, SampleData};
use anyhow::{Context, Result, anyhow, bail};
use crossbeam_channel::{Receiver, Sender, unbounded};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub const AUDIO_EXTENSIONS: [&str; 10] = [
    "wav", "wave", "aif", "aiff", "mp3", "m4a", "aac", "flac", "ogg", "caf",
];

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum MidiKind {
    Note,
    Cc,
}

/// Contrôle MIDI associé à un pad.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct MidiKey {
    pub kind: MidiKind,
    pub channel: u8,
    pub number: u8,
}

impl MidiKey {
    pub fn label(&self) -> String {
        match self.kind {
            MidiKind::Note => format!("{} · ch{}", note_name(self.number), self.channel + 1),
            MidiKind::Cc => format!("CC{} · ch{}", self.number, self.channel + 1),
        }
    }
}

pub fn note_name(n: u8) -> String {
    const N: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    format!("{}{}", N[(n % 12) as usize], (n as i32 / 12) - 1)
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Pad {
    pub name: String,
    pub path: PathBuf,
    #[serde(default)]
    pub color: u8,
    // Nombres : `null` (NaN écrit par serde_json) ne rend pas tout le soundboard illisible.
    #[serde(default, deserialize_with = "lenient_f32")]
    pub gain_db: f32,
    /// Début (s).
    #[serde(default, deserialize_with = "lenient_f64")]
    pub start: f64,
    /// Fin (s). `None` = fin du fichier.
    #[serde(default)]
    pub end: Option<f64>,
    #[serde(default, deserialize_with = "lenient_f64")]
    pub fade_in: f64,
    #[serde(default, deserialize_with = "lenient_f64")]
    pub fade_out: f64,
    /// Valeurs inconnues (version plus récente) : réglage par défaut, fichier protégé.
    #[serde(default, deserialize_with = "lenient")]
    pub mode: PadMode,
    #[serde(default)]
    pub looped: bool,
    /// Nom de touche clavier (egui::Key::name).
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub midi: Option<MidiKey>,
    /// Champs inconnus (version plus récente) : réécrits tels quels.
    #[serde(flatten)]
    pub extra: Extra,
}

impl Pad {
    pub fn new(path: PathBuf, color: u8) -> Self {
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Son".into());
        Self {
            name,
            path,
            color,
            gain_db: 0.0,
            start: 0.0,
            end: None,
            fade_in: 0.0,
            fade_out: 0.0,
            // Nouveau pad : chaque appui relance le son depuis le début.
            mode: PadMode::Retrigger,
            looped: false,
            key: None,
            midi: None,
            extra: Extra::new(),
        }
    }

    /// Paramètres de lecture pour un son chargé.
    pub fn params(&self, s: &SampleData) -> PadParams {
        let sr = s.sample_rate as f64;
        let to = |t: f64| ((t.max(0.0) * sr) as usize).min(s.frames);
        let start = to(self.start);
        let end = self.end.map_or(s.frames, to).max(start);
        let len = end - start;
        PadParams {
            start,
            end,
            fade_in: to(self.fade_in).min(len),
            fade_out: to(self.fade_out).min(len),
            gain: db_to_gain(self.gain_db),
            mode: self.mode,
            looped: self.looped,
        }
    }
}

pub fn db_to_gain(db: f32) -> f32 {
    // Valeur abîmée (NaN) : gain neutre ; au-delà de +24 dB, borné (fichier fabriqué).
    if db.is_nan() {
        1.0
    } else if db <= -60.0 {
        0.0
    } else {
        10f32.powf(db.min(24.0) / 20.0)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Page {
    pub name: String,
    /// Un pad illisible (version plus récente, fichier abîmé) laisse sa case vide au lieu de
    /// rendre tout le soundboard illisible ; le fichier est alors protégé.
    #[serde(deserialize_with = "lenient_pads")]
    pub pads: Vec<Option<Pad>>,
    /// Champs inconnus (version plus récente) : réécrits tels quels.
    #[serde(flatten)]
    pub extra: Extra,
}

impl Page {
    pub fn new(name: String) -> Self {
        Self {
            name,
            pads: vec![None; PADS_PER_PAGE],
            extra: Extra::new(),
        }
    }
}

/// Pads d'une page, chacun lu à part (voir `Page::pads`).
fn lenient_pads<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<Option<Pad>>, D::Error> {
    let raw = Vec::<serde_json::Value>::deserialize(d)?;
    Ok(raw
        .into_iter()
        .map(|v| crate::project::lenient(v).unwrap_or(None))
        .collect())
}

/// Le fichier de soundboard de la session ne doit pas être écrasé : illisible, ou écrit par
/// une version plus récente. Le soundboard affiché est celui d'alors (`snapshot`) : tant
/// qu'il n'est pas modifié, rien n'est écrit ; à la première vraie modification, le fichier
/// est d'abord mis de côté sous un autre nom (voir `Board::release_guard`).
#[derive(Clone, Debug)]
pub struct BoardGuard {
    pub reason: String,
    snapshot: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Board {
    /// Version du format (toujours écrite à `FORMAT_VERSION`).
    #[serde(default, serialize_with = "write_format_version")]
    pub format_version: u32,
    pub cols: usize,
    pub rows: usize,
    pub pages: Vec<Page>,
    /// Ancien emplacement de la sortie du soundboard (désormais dans les réglages audio,
    /// `AudioPrefs::pad_output` / `pad_left`) : lu une fois pour reprendre le réglage.
    #[serde(default)]
    pub out_left: u16,
    /// Ancien emplacement, voir `out_left`.
    #[serde(default)]
    pub out_device: Option<String>,
    #[serde(default)]
    pub midi_port: Option<String>,
    /// Ancien « mode de la page » (le mode se règle maintenant pad par pad) : conservé
    /// pour relire les anciens fichiers, sans effet.
    #[serde(default, deserialize_with = "lenient")]
    pub default_mode: PadMode,
    /// LED du contrôleur.
    #[serde(default, deserialize_with = "lenient")]
    pub led_mode: crate::midi::LedMode,
    /// Port de sortie des LED (`None` = celui du contrôleur choisi).
    #[serde(default)]
    pub led_port: Option<String>,
    /// Disposition : calquée sur la grille du contrôleur quand il en a une (chaque case à
    /// l'écran = le pad physique au même endroit), sinon MIDI learn pad par pad.
    #[serde(default, deserialize_with = "lenient")]
    pub layout: PadLayout,
    /// Mode spectacle : pads jouables, édition verrouillée (suppression, déplacement,
    /// remplacement, réglages, pages). Gardé avec la session.
    #[serde(default)]
    pub locked: bool,
    /// Sons sur un partage réseau désignés par le fichier de session lui-même (hors de la
    /// session) : jamais ouverts automatiquement (voir `load_session`).
    #[serde(skip)]
    pub network_pads: Vec<PathBuf>,
    /// Anti-rebond des pads « Lecture-Stop » (ms, 0 = désactivé).
    #[serde(default = "default_debounce")]
    pub debounce_ms: u32,
    /// Champs inconnus (version plus récente) : réécrits tels quels.
    #[serde(flatten)]
    pub extra: Extra,
    /// Fichier de la session à ne pas écraser (voir `BoardGuard`).
    #[serde(skip)]
    pub guard: Option<BoardGuard>,
}

fn default_debounce() -> u32 {
    150
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum PadLayout {
    /// Grille du contrôleur dès qu'un contrôleur à grille reconnu est branché (le MIDI
    /// appartient à la case, la page affichée est jouée, la colonne de droite change de
    /// page) ; sinon grille libre avec MIDI learn.
    // Anciennes valeurs (« Free » était le défaut) : on repasse en automatique.
    #[default]
    #[serde(alias = "Free", alias = "Controller")]
    Auto,
    /// Toujours le MIDI learn pad par pad, même avec un contrôleur à grille.
    Manual,
}

impl Default for Board {
    fn default() -> Self {
        Self {
            cols: 6,
            rows: 4,
            pages: vec![Page::new("Page 1".into())],
            out_left: 0,
            out_device: None,
            midi_port: None,
            default_mode: PadMode::Toggle,
            led_mode: crate::midi::LedMode::Auto,
            led_port: None,
            layout: PadLayout::Auto,
            locked: false,
            network_pads: Vec::new(),
            debounce_ms: default_debounce(),
            format_version: FORMAT_VERSION,
            extra: Extra::new(),
            guard: None,
        }
    }
}

/// Fichier du soundboard dans le dossier d'une session.
pub const BOARD_FILE: &str = "soundboard.json";
/// Dossier des sons du soundboard dans une session (copies des fichiers déposés).
pub const SOUNDS_DIR: &str = "Sons";

impl Board {
    /// Réglages du contrôleur, communs à toutes les sessions (ce sont ceux de l'ordinateur) :
    /// `%APPDATA%\TracksPlus\soundboard.json`.
    pub fn default_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("TracksPlus").join("soundboard.json"))
    }

    /// Copie de l'ancien soundboard commun, faite une fois avant que les soundboards ne
    /// passent dans les sessions (pour pouvoir l'importer).
    pub fn legacy_backup_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| {
            d.join("TracksPlus")
                .join("soundboard (avant les sessions).json")
        })
    }

    /// Réglages communs : ceux du fichier de l'ordinateur. Un ancien soundboard commun qui
    /// contient des pads est d'abord mis de côté (une seule fois) pour rester importable.
    pub fn load_devices() -> Self {
        let b = Self::load_default();
        if b.all_pads().next().is_some()
            && let (Some(src), Some(dst)) = (Self::default_path(), Self::legacy_backup_path())
            && !dst.exists()
        {
            let _ = std::fs::copy(src, dst);
        }
        let mut d = Board::default();
        d.take_devices(&b);
        d
    }

    /// Enregistre les réglages communs (sans les pads).
    pub fn save_devices(&self) -> Result<()> {
        let mut d = Board::default();
        d.take_devices(self);
        match Self::default_path() {
            Some(p) => d.save(&p),
            None => Ok(()),
        }
    }

    /// Reprend les réglages de l'ordinateur (contrôleur, LED, disposition, sortie) d'un autre
    /// soundboard.
    pub fn take_devices(&mut self, from: &Board) {
        self.midi_port = from.midi_port.clone();
        self.led_mode = from.led_mode;
        self.led_port = from.led_port.clone();
        self.layout = from.layout;
        self.out_left = from.out_left;
        self.out_device = from.out_device.clone();
    }

    /// Soundboard de la session `dir` (vide s'il n'y en a pas encore). Les chemins des sons
    /// sont relatifs au dossier de la session dans le fichier : on les rend absolus.
    /// Un fichier illisible est une erreur (jamais un soundboard vide qui l'écraserait) ; un
    /// fichier d'une version plus récente est lu, protégé (`guard`).
    pub fn load_session(dir: &Path) -> Result<Self> {
        let path = dir.join(BOARD_FILE);
        // `exists()` répondrait « absent » sur une erreur d'accès : le soundboard serait vu
        // comme neuf, puis écrasé.
        match path.try_exists() {
            Ok(false) => return Ok(Board::default()),
            Ok(true) => {}
            Err(e) => {
                return Err(anyhow::Error::new(e).context(format!("Accès à {}", path.display())));
            }
        }
        crate::project::clean_stale_temps(&path);
        let (mut b, newer) = Self::load_checked(&path)?;
        b.resolve_paths(dir);
        if newer {
            b.protect("soundboard d'une version plus récente".into());
        }
        Ok(b)
    }

    /// Chemins des sons d'un soundboard lu dans un fichier du dossier `dir` (session reçue,
    /// soundboard importé) : les chemins relatifs deviennent absolus. Les sons qu'un fichier
    /// venu d'ailleurs ne doit pas faire ouvrir tout seul sont mis de côté (`network_pads`,
    /// jamais ouverts ni copiés) :
    /// - sur un partage réseau : ouvrir `\\serveur\…` envoie l'identifiant Windows (chiffré)
    ///   à ce serveur ;
    /// - relatifs mais hors du dossier (`..\..\.ssh\…`) : « Enregistrer sous » copierait
    ///   n'importe quel fichier de l'ordinateur dans la session.
    pub fn resolve_paths(&mut self, dir: &Path) {
        let mut parked = Vec::new();
        for page in &mut self.pages {
            for pad in page.pads.iter_mut().flatten() {
                if is_network_path(&pad.path) || escapes_dir(&pad.path) {
                    parked.push(pad.path.clone());
                } else if pad.path.is_relative() {
                    pad.path = dir.join(&pad.path);
                }
            }
        }
        self.network_pads = parked;
    }

    /// Protège le fichier de soundboard de la session tel qu'il est sur le disque (voir
    /// `BoardGuard`), le soundboard affiché étant celui d'à présent.
    pub fn protect(&mut self, reason: String) {
        self.guard = None;
        let snapshot = self.content_key();
        self.guard = Some(BoardGuard { reason, snapshot });
    }

    /// Contenu propre à la session (sans les réglages de l'ordinateur), pour savoir si le
    /// soundboard a vraiment été modifié.
    fn content_key(&self) -> String {
        let mut c = self.clone();
        c.guard = None;
        c.take_devices(&Board::default());
        serde_json::to_string(&c).unwrap_or_default()
    }

    /// Soundboard protégé modifié depuis (voir `BoardGuard`).
    pub fn changed_since_guard(&self) -> bool {
        self.guard
            .as_ref()
            .is_some_and(|g| g.snapshot != self.content_key())
    }

    /// Fin de la protection, avant d'écrire un soundboard modifié : le fichier protégé de la
    /// session `dir` est mis de côté (`soundboard.illisible-AAAAMMJJ-HHMMSS.json`, importable)
    /// sans rien écraser. Renvoie son nouveau chemin (`None` : plus de fichier).
    pub fn release_guard(&mut self, dir: &Path) -> Result<Option<PathBuf>> {
        if self.guard.is_none() {
            return Ok(None);
        }
        let path = dir.join(BOARD_FILE);
        let aside = match crate::project::set_aside(&path, "illisible", true) {
            Ok(p) => Some(p),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(anyhow::Error::new(e).context("Mise de côté de l'ancien soundboard"));
            }
        };
        self.guard = None;
        Ok(aside)
    }

    /// Enregistre le soundboard dans la session `dir`, avec des chemins relatifs pour les
    /// sons du dossier de la session (la session reste valable si on la déplace). Refusé
    /// tant que le fichier en place est protégé (`guard`).
    pub fn save_session(&self, dir: &Path) -> Result<()> {
        if let Some(g) = &self.guard {
            bail!("{} · fichier protégé", g.reason);
        }
        let mut b = self.clone();
        for page in &mut b.pages {
            for pad in page.pads.iter_mut().flatten() {
                if let Ok(rel) = pad.path.strip_prefix(dir) {
                    pad.path = rel.to_path_buf();
                }
            }
        }
        b.save(&dir.join(BOARD_FILE))
    }

    /// Copie les sons utilisés dans le dossier `Sons` de la session `dir` (ceux qui n'y sont
    /// pas déjà) et fait pointer les pads vers ces copies. Renvoie les sons introuvables.
    pub fn adopt_sounds(&mut self, dir: &Path) -> Vec<PathBuf> {
        let mut missing = Vec::new();
        let mut done: std::collections::HashMap<PathBuf, PathBuf> = Default::default();
        for page in &mut self.pages {
            for pad in page.pads.iter_mut().flatten() {
                if let Some(p) = done.get(&pad.path) {
                    pad.path = p.clone();
                    continue;
                }
                // Son réseau ou hors de la session imposé par un fichier : jamais lu
                // automatiquement. Et seuls des sons sont copiés : un pad qui désigne un
                // autre fichier (clé, document) n'est pas recopié dans la session.
                if self.network_pads.contains(&pad.path) || !is_audio_file(&pad.path) {
                    missing.push(pad.path.clone());
                    continue;
                }
                match copy_into_session(&pad.path, dir) {
                    Ok(p) => {
                        done.insert(pad.path.clone(), p.clone());
                        pad.path = p;
                    }
                    Err(_) => missing.push(pad.path.clone()),
                }
            }
        }
        missing
    }

    pub fn load(path: &Path) -> Result<Self> {
        Self::load_checked(path).map(|(b, _)| b)
    }

    /// Comme `load` ; `true` si le fichier vient d'une version plus récente (numéro de
    /// format, ou valeurs inconnues remplacées par défaut) : à ne pas réécrire tel quel.
    pub fn load_checked(path: &Path) -> Result<(Self, bool)> {
        let s = crate::project::read_retrying(path)
            .with_context(|| format!("Lecture de {}", path.display()))?;
        let (mut b, unknown): (Board, bool) = match parse_json(&s) {
            Ok(r) => r,
            Err(e) => {
                if let Some(v) = crate::project::newer_format(&s) {
                    bail!("soundboard d'une version plus récente (format {v})");
                }
                return Err(anyhow::Error::new(e).context("Fichier de soundboard invalide"));
            }
        };
        b.normalize();
        let newer = unknown || b.format_version > FORMAT_VERSION;
        Ok((b, newer))
    }

    pub fn load_default() -> Self {
        Self::default_path()
            .and_then(|p| Self::load(&p).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        crate::project::write_atomic(path, serde_json::to_string_pretty(self)?.as_bytes())
    }

    pub fn has_pads(&self) -> bool {
        self.all_pads().next().is_some()
    }

    pub fn normalize(&mut self) {
        self.cols = self.cols.clamp(1, 8);
        self.rows = self.rows.clamp(1, 8);
        if self.pages.is_empty() {
            self.pages.push(Page::new("Page 1".into()));
        }
        self.pages.truncate(MAX_PAGES);
        for p in &mut self.pages {
            p.pads.resize(PADS_PER_PAGE, None);
        }
    }

    pub fn slot(page: usize, idx: usize) -> u16 {
        (page * PADS_PER_PAGE + idx) as u16
    }

    pub fn pad(&self, slot: u16) -> Option<&Pad> {
        let (p, i) = (slot as usize / PADS_PER_PAGE, slot as usize % PADS_PER_PAGE);
        self.pages.get(p)?.pads.get(i)?.as_ref()
    }

    pub fn pad_mut(&mut self, slot: u16) -> Option<&mut Pad> {
        let (p, i) = (slot as usize / PADS_PER_PAGE, slot as usize % PADS_PER_PAGE);
        self.pages.get_mut(p)?.pads.get_mut(i)?.as_mut()
    }

    pub fn set(&mut self, slot: u16, pad: Option<Pad>) {
        let (p, i) = (slot as usize / PADS_PER_PAGE, slot as usize % PADS_PER_PAGE);
        if let Some(x) = self.pages.get_mut(p).and_then(|pg| pg.pads.get_mut(i)) {
            *x = pad;
        }
    }

    pub fn all_pads(&self) -> impl Iterator<Item = (u16, &Pad)> {
        self.pages.iter().enumerate().flat_map(|(pi, pg)| {
            pg.pads
                .iter()
                .enumerate()
                .filter_map(move |(i, p)| p.as_ref().map(|p| (Board::slot(pi, i), p)))
        })
    }
}

// ------------------------------------------------------------------ chargement

pub enum LoadState {
    Loading,
    Ready(Arc<SampleData>),
    Failed(String),
}

/// Nombre de threads de décodage : assez pour charger vite un soundboard entier, sans
/// saturer le processeur (ni la mémoire) en plein spectacle.
const DECODE_WORKERS: usize = 2;

/// Les sons sont chargés en mémoire : au-delà de 30 min, on refuse plutôt que de risquer
/// de saturer la mémoire de la machine en plein spectacle.
const MAX_SECONDS: u64 = 30 * 60;

/// Taille maximale d'un son décodé (stéréo f32 à la fréquence de la carte) : 30 min à
/// 192 kHz y tiennent ; au-delà (fréquence de session extrême), le pad est refusé.
pub const MAX_PAD_BYTES: u64 = 3 << 30;

/// Sons à l'ancienne fréquence gardés après un changement de fréquence de la carte : une
/// reconnexion qui revient aussitôt à la fréquence d'avant ne redécode rien.
const STASH_KEEP: std::time::Duration = std::time::Duration::from_secs(120);

const MEMORY_FULL: &str = "Mémoire des sons pleine · retirez des pads";

/// Mémoire occupée par un son décodé de `frames` trames (données et forme d'onde).
fn sample_bytes(frames: u64) -> u64 {
    frames.saturating_mul(8) + (frames / SAMPLE_PEAK as u64 + 1) * 8
}

/// Mémoire réservée aux sons des pads, partagée avec les threads de décodage : un son
/// n'est réservé (puis alloué) que s'il y tient. Sans cette limite, un soundboard trop
/// gros finirait par faire échouer une allocation ordinaire (interface, enregistreur), ce
/// qui arrête l'application.
pub struct Budget {
    limit: u64,
    /// Sons prêts (ceux du cache, comptés par l'interface).
    committed: AtomicU64,
    /// Sons en cours de décodage, déjà réservés.
    inflight: AtomicU64,
}

impl Budget {
    fn new(limit: u64) -> Self {
        Self {
            limit,
            committed: AtomicU64::new(0),
            inflight: AtomicU64::new(0),
        }
    }

    /// Par défaut : 40 % de la mémoire de l'ordinateur (au moins 1 Go).
    fn default_limit() -> u64 {
        crate::isolate::physical_memory()
            .map_or(4 << 30, |m| m / 5 * 2)
            .max(1 << 30)
    }

    fn try_acquire(&self, bytes: u64) -> bool {
        let committed = self.committed.load(Ordering::Relaxed);
        self.inflight
            .try_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
                (committed.saturating_add(cur).saturating_add(bytes) <= self.limit)
                    .then_some(cur + bytes)
            })
            .is_ok()
    }

    fn release(&self, bytes: u64) {
        let _ = self
            .inflight
            .try_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
                Some(cur.saturating_sub(bytes))
            });
    }

    /// Passe une réservation de `from` à `to` octets : réduite toujours, agrandie
    /// seulement s'il y a la place (sinon elle reste de `from`).
    fn resize(&self, from: u64, to: u64) -> bool {
        if to <= from {
            self.release(from - to);
            true
        } else {
            self.try_acquire(to - from)
        }
    }

    /// Place encore libre.
    fn available(&self) -> u64 {
        let used = self
            .committed
            .load(Ordering::Relaxed)
            .saturating_add(self.inflight.load(Ordering::Relaxed));
        self.limit.saturating_sub(used)
    }
}

/// Demande de décodage : chemin, fréquence cible, génération, fenêtre à rafraîchir.
type DecodeJob = (PathBuf, u32, u64, Option<eframe::egui::Context>);

/// Résultat d'un décodage.
struct Decoded {
    path: PathBuf,
    generation: u64,
    result: Result<SampleData, String>,
    /// Mémoire réservée pour ce son (rendue quand il entre dans le cache, ou est oublié).
    reserved: u64,
    /// Refusé faute de place dans la mémoire réservée aux sons.
    full: bool,
}

/// Génération attendue par chemin (une demande remplacée ou retirée est ignorée).
type Pending = Arc<parking_lot::Mutex<HashMap<PathBuf, u64>>>;

/// Cache des sons chargés, partagé par tous les pads.
pub struct SampleCache {
    pub map: HashMap<PathBuf, LoadState>,
    sample_rate: u32,
    tx: Sender<Decoded>,
    rx: Receiver<Decoded>,
    /// File des décodages, démarrée à la première demande.
    jobs: Option<Sender<DecodeJob>>,
    pending: Pending,
    next_gen: u64,
    budget: Arc<Budget>,
    /// Sons mis de côté par sécurité (partage réseau, chemin hors de la session) : jamais
    /// rouverts automatiquement, même après un changement de fréquence.
    parked: HashSet<PathBuf>,
    /// Sons à la fréquence précédente (voir `STASH_KEEP`).
    stash: Option<(u32, HashMap<PathBuf, Arc<SampleData>>, std::time::Instant)>,
    /// Dernière génération demandée quand ces sons ont été libérés : un refus « mémoire
    /// pleine » d'une demande antérieure a pu être décidé alors qu'ils comptaient encore.
    stash_dropped_gen: u64,
    /// Nombre d'entrées lors du dernier décompte de la mémoire.
    counted_len: usize,
    scratch: Vec<usize>,
}

impl SampleCache {
    pub fn new(sample_rate: u32) -> Self {
        Self::with_budget(sample_rate, Budget::default_limit())
    }

    fn with_budget(sample_rate: u32, limit: u64) -> Self {
        let (tx, rx) = unbounded();
        Self {
            map: HashMap::new(),
            sample_rate,
            tx,
            rx,
            jobs: None,
            pending: Default::default(),
            next_gen: 0,
            budget: Arc::new(Budget::new(limit)),
            parked: HashSet::new(),
            stash: None,
            stash_dropped_gen: 0,
            counted_len: 0,
            scratch: Vec::new(),
        }
    }

    /// Met un son de côté sans jamais l'ouvrir (sécurité, voir `Board::resolve_paths`).
    pub fn park(&mut self, path: &Path) {
        let why = if is_network_path(path) {
            "Son sur un partage réseau, hors de la session : remplacez-le (clic droit) pour le copier dans la session"
        } else {
            "Chemin du son hors de la session (fichier reçu d'ailleurs) : remplacez-le (clic droit) pour le copier dans la session"
        };
        self.pending.lock().remove(path);
        self.parked.insert(path.to_path_buf());
        self.map
            .insert(path.to_path_buf(), LoadState::Failed(why.into()));
    }

    pub fn set_sample_rate(&mut self, sr: u32) {
        if sr == self.sample_rate {
            return;
        }
        let old_rate = std::mem::replace(&mut self.sample_rate, sr);
        let paths: Vec<PathBuf> = self.map.keys().cloned().collect();
        let mut old: HashMap<PathBuf, Arc<SampleData>> = HashMap::new();
        let mut parked = Vec::new();
        for (p, st) in self.map.drain() {
            match st {
                // Sons mis de côté par sécurité : restent indisponibles (jamais rouverts
                // par un simple changement de fréquence).
                st if self.parked.contains(&p) => parked.push((p, st)),
                LoadState::Ready(s) => {
                    old.insert(p, s);
                }
                _ => {}
            }
        }
        self.pending.lock().clear();
        self.map.extend(parked);
        // Retour à la fréquence d'avant (reconnexion) : sons repris sans décodage.
        let stash = self.stash.take().filter(|(rate, _, _)| *rate == sr);
        for p in paths {
            if self.map.contains_key(&p) {
                continue;
            }
            match stash.as_ref().and_then(|(_, m, _)| m.get(&p)) {
                Some(s) => {
                    self.map.insert(p, LoadState::Ready(s.clone()));
                }
                None => self.request(&p, None),
            }
        }
        // Les sons à l'ancienne fréquence restent un moment de côté, si la mémoire le
        // permet (au plus la moitié de la mémoire réservée aux sons).
        let bytes: u64 = old.values().map(|s| Self::bytes_of(s)).sum();
        if !old.is_empty() && bytes.saturating_mul(2) <= self.budget.limit {
            self.stash = Some((old_rate, old, std::time::Instant::now()));
        }
        self.recount();
    }

    fn bytes_of(s: &SampleData) -> u64 {
        (s.data.capacity() as u64) * 4 + (s.peaks.capacity() as u64) * 8
    }

    /// Recompte la mémoire des sons prêts (un même son sous deux chemins compte une fois).
    fn recount(&mut self) {
        self.scratch.clear();
        let mut total = 0u64;
        let ready = self.map.values().filter_map(|st| match st {
            LoadState::Ready(s) => Some(s),
            _ => None,
        });
        let stashed = self.stash.iter().flat_map(|(_, m, _)| m.values());
        for s in ready.chain(stashed) {
            let ptr = Arc::as_ptr(s) as usize;
            if !self.scratch.contains(&ptr) {
                self.scratch.push(ptr);
                total += Self::bytes_of(s);
            }
        }
        self.budget.committed.store(total, Ordering::Relaxed);
        self.counted_len = self.map.len();
    }

    /// Threads de décodage (au plus `DECODE_WORKERS`), arrêtés avec le cache.
    fn workers(&mut self) -> &Sender<DecodeJob> {
        self.jobs.get_or_insert_with(|| {
            let (jtx, jrx) = unbounded::<DecodeJob>();
            for i in 0..DECODE_WORKERS {
                let jrx = jrx.clone();
                let tx = self.tx.clone();
                let pending = self.pending.clone();
                let budget = self.budget.clone();
                let _ = std::thread::Builder::new()
                    .name(format!("tracksplus-decode-{i}"))
                    .spawn(move || {
                        for (path, sr, generation, ctx) in jrx {
                            // Demande remplacée ou retirée entre-temps : rien à décoder.
                            if pending.lock().get(&path) != Some(&generation) {
                                continue;
                            }
                            let (result, reserved, full) = decode_job(&path, sr, &budget);
                            let d = Decoded {
                                path,
                                generation,
                                result,
                                reserved,
                                full,
                            };
                            if let Err(e) = tx.send(d) {
                                budget.release(e.into_inner().reserved);
                            }
                            if let Some(c) = ctx {
                                c.request_repaint();
                            }
                        }
                    });
            }
            jtx
        })
    }

    pub fn request(&mut self, path: &Path, ctx: Option<&eframe::egui::Context>) {
        if self.map.contains_key(path) {
            return;
        }
        self.map.insert(path.to_path_buf(), LoadState::Loading);
        self.next_gen += 1;
        let generation = self.next_gen;
        self.pending.lock().insert(path.to_path_buf(), generation);
        let job = (
            path.to_path_buf(),
            self.sample_rate,
            generation,
            ctx.cloned(),
        );
        if self.workers().send(job).is_err() {
            self.pending.lock().remove(path);
            self.map.insert(
                path.to_path_buf(),
                LoadState::Failed("Chargement impossible".into()),
            );
        }
    }

    /// Même son sous un autre chemin (fichier copié par « Enregistrer sous ») : aucun
    /// nouveau décodage, le pad reste jouable sans interruption.
    pub fn alias(&mut self, from: &Path, to: &Path) {
        if from == to || self.map.contains_key(to) {
            return;
        }
        if let Some(LoadState::Ready(s)) = self.map.get(from) {
            let s = s.clone();
            self.map.insert(to.to_path_buf(), LoadState::Ready(s));
        }
    }

    pub fn reload(&mut self, path: &Path) {
        if self.parked.contains(path) {
            return;
        }
        self.map.remove(path);
        self.request(path, None);
        self.recount();
    }

    /// Oublie les sons qui ne sont plus utilisés par aucun pad. Les sons en cours de
    /// lecture ne sont pas coupés : chaque voix garde son propre accès au son.
    pub fn retain(&mut self, keep: &std::collections::HashSet<PathBuf>) {
        self.map.retain(|p, _| keep.contains(p));
        self.pending.lock().retain(|p, _| keep.contains(p));
        self.parked.retain(|p| keep.contains(p));
        if let Some((_, m, _)) = &mut self.stash {
            m.retain(|p, _| keep.contains(p));
        }
        self.recount();
    }

    pub fn poll(&mut self) -> bool {
        let mut any = false;
        let mut retry = Vec::new();
        while let Ok(d) = self.rx.try_recv() {
            let current = {
                let mut pending = self.pending.lock();
                let ok = pending.get(&d.path) == Some(&d.generation);
                if ok {
                    pending.remove(&d.path);
                }
                ok
            };
            if !current {
                self.budget.release(d.reserved);
                continue;
            }
            any = true;
            // Mémoire pleine alors que des sons à l'ancienne fréquence sont gardés de
            // côté : ils passent après les sons utiles. Tout refus d'une demande faite
            // avant leur libération est relancé (dans ce lot ou un suivant), pas seulement
            // le premier : la place a pu manquer à cause d'eux. Une demande relancée a une
            // génération plus récente : un nouveau refus est alors définitif.
            if d.full && (self.stash.is_some() || d.generation <= self.stash_dropped_gen) {
                self.drop_stash();
                self.map.remove(&d.path);
                retry.push(d.path);
                continue;
            }
            self.map.insert(
                d.path,
                match d.result {
                    Ok(s) => LoadState::Ready(Arc::new(s)),
                    Err(e) => LoadState::Failed(e),
                },
            );
            // Compté dans les sons prêts avant d'être rendu par la réservation.
            self.recount();
            self.budget.release(d.reserved);
        }
        if !retry.is_empty() {
            self.recount();
            for p in retry {
                self.request(&p, None);
            }
        }
        if self
            .stash
            .as_ref()
            .is_some_and(|(_, _, at)| at.elapsed() > STASH_KEEP)
        {
            self.drop_stash();
            self.recount();
        } else if self.map.len() != self.counted_len {
            self.recount();
        }
        any
    }

    /// Libère les sons à l'ancienne fréquence (le décompte suit, par l'appelant).
    fn drop_stash(&mut self) {
        if self.stash.take().is_some() {
            self.stash_dropped_gen = self.next_gen;
        }
    }

    pub fn get(&self, path: &Path) -> Option<&Arc<SampleData>> {
        match self.map.get(path) {
            Some(LoadState::Ready(s)) => Some(s),
            _ => None,
        }
    }
}

/// Première lettre en capitale (messages affichés sur un pad).
fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

/// Décode un son pour un pad : dans un processus fils quand c'est possible (un fichier
/// piégé n'arrête que lui), sinon ici. La mémoire réservée aux sons est vérifiée dès que
/// la taille du son est connue (en-tête), avant de le décoder, puis ajustée à sa taille
/// réelle avant de l'allouer ici. Rend (résultat, mémoire réservée, refus faute de place).
fn decode_job(path: &Path, sr: u32, budget: &Budget) -> (Result<SampleData, String>, u64, bool) {
    let reserved = std::cell::Cell::new(0u64);
    let full = std::cell::Cell::new(false);
    let accept = |frames: u64| -> Result<(), String> {
        if frames.saturating_mul(8) > MAX_PAD_BYTES {
            return Err("son trop long à cette fréquence de session".into());
        }
        let b = sample_bytes(frames);
        if !budget.resize(reserved.get(), b) {
            full.set(true);
            return Err(MEMORY_FULL.into());
        }
        reserved.set(b);
        Ok(())
    };
    // Mémoire du processus fils : le son décodé et sa marge de croissance, sans dépasser
    // ce que la mémoire réservée aux sons peut encore accueillir (un son plus gros y serait
    // refusé de toute façon), plus le décodeur lui-même.
    let room = budget.available().saturating_mul(13) / 10;
    let need = (MAX_PAD_BYTES + MAX_PAD_BYTES / 4).min(room) + (512 << 20);
    let r = match crate::isolate::decode_pad(path, sr, need, &accept) {
        Some(r) => r.and_then(|data| build_sample(data, sr).map_err(|e| format!("{e:#}"))),
        None => decode_guarded(path, sr, &accept).and_then(|s| accept(s.frames as u64).map(|_| s)),
    };
    if r.is_err() && reserved.get() > 0 {
        budget.release(reserved.get());
        reserved.set(0);
    }
    (r.map_err(|e| capitalize(&e)), reserved.get(), full.get())
}

/// Décodage protégé : un décodeur qui plante sur un fichier abîmé donne une erreur
/// (le pad l'affiche) au lieu de laisser le pad « en chargement » pour toujours.
/// `accept` reçoit la taille annoncée par l'en-tête (voir `decode_stereo`).
fn decode_guarded(
    path: &Path,
    sr: u32,
    accept: &dyn Fn(u64) -> Result<(), String>,
) -> Result<SampleData, String> {
    let announce = |frames: u64| accept(frames).map_err(|e| anyhow!(e));
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let data = decode_stereo(path, sr, &|_| {}, &announce)?;
        build_sample(shrink(data), sr)
    })) {
        Ok(r) => r.map_err(|e| format!("{e:#}")),
        Err(_) => Err("fichier illisible".into()),
    }
}

/// Décode un fichier audio en stéréo f32 à la fréquence `target_sr`, ici et sans mémoire
/// réservée (tests ; l'application passe par `decode_job`).
#[cfg(test)]
pub fn load_sample(path: &Path, target_sr: u32) -> Result<SampleData> {
    let data = decode_stereo(path, target_sr, &|_| {}, &|_| Ok(()))?;
    build_sample(shrink(data), target_sr)
}

/// Son prêt à jouer : données stéréo entrelacées et forme d'onde.
fn build_sample(data: Vec<f32>, sample_rate: u32) -> Result<SampleData> {
    let frames = data.len() / 2;
    if frames == 0 {
        return Err(anyhow!("fichier vide"));
    }
    let mut peaks = Vec::new();
    peaks
        .try_reserve_exact(frames / SAMPLE_PEAK + 1)
        .map_err(|_| anyhow!("mémoire insuffisante"))?;
    for block in data.chunks(SAMPLE_PEAK * 2) {
        let (mut lo, mut hi) = (f32::INFINITY, f32::NEG_INFINITY);
        for &[l, r] in block.as_chunks::<2>().0 {
            lo = lo.min(l.min(r));
            hi = hi.max(l.max(r));
        }
        // Premier point du bloc toujours inclus (bloc d'une seule trame).
        let m = 0.5 * (block[0] + block[1]);
        peaks.push([lo.min(m), hi.max(m)]);
    }
    Ok(SampleData {
        sample_rate,
        frames,
        data,
        peaks,
    })
}

/// Décode `path` en stéréo entrelacé f32 à `target_sr` (rééchantillonné au fil du
/// décodage : jamais l'original et la version convertie en mémoire en même temps). Toutes
/// les réservations sont faillibles et bornées : un manque de mémoire ou un en-tête
/// fantaisiste devient une erreur affichée sur le pad, jamais un arrêt de l'application.
/// `tick(f)` reçoit l'avancement (0 à 1, quand la longueur est connue). `announce(trames)`
/// reçoit la taille attendue (si l'en-tête la donne) avant toute réservation ; son refus
/// arrête le décodage.
pub fn decode_stereo(
    path: &Path,
    target_sr: u32,
    tick: &dyn Fn(f64),
    announce: &dyn Fn(u64) -> Result<()>,
) -> Result<Vec<f32>> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::errors::Error as SymError;

    if !crate::decode::VALID_SOURCE_RATES.contains(&target_sr) {
        return Err(anyhow!("fréquence de la carte invalide ({target_sr} Hz)"));
    }
    let crate::decode::Opened {
        mut format,
        mut decoder,
        track_id,
        sample_rate,
        n_frames,
        mut trim,
        file_len,
        ..
    } = crate::decode::open(path)?;
    let src_sr = sample_rate.unwrap_or(target_sr);
    if let Some(n) = n_frames
        && n / src_sr as u64 > MAX_SECONDS
    {
        return Err(anyhow!(
            "son trop long ({} min, max 30)",
            n / src_sr as u64 / 60
        ));
    }
    let max_src = MAX_SECONDS * src_sr as u64;
    let mut conv = crate::decode::Converter::new(2, src_sr, target_sr)?;
    let max_out = conv.output_frames(max_src).min(MAX_PAD_BYTES / 8);
    if let Some(n) = n_frames
        && conv.output_frames(n) > max_out
    {
        return Err(anyhow!("son trop long à cette fréquence de session"));
    }
    let oom = |_| anyhow!("mémoire insuffisante");
    let mut data: Vec<f32> = Vec::new();
    // Réservation d'après l'en-tête, bornée par la taille réelle du fichier (au plus 64
    // trames par octet, bien au-delà de tout format réel) : un fichier de 100 octets qui
    // annonce des heures ne fait rien réserver. Le reste grandit au besoin.
    if let Some(n) = n_frames {
        let n = n.min(max_src).min(file_len.saturating_mul(64));
        let frames = conv.output_frames(n).min(max_out);
        // Taille annoncée avant toute réservation : la mémoire réservée aux sons peut
        // refuser le son tout de suite, sans le décoder en entier.
        announce(frames)?;
        let want = frames + crate::decode::CHUNK as u64;
        data.try_reserve_exact(want as usize * 2).map_err(oom)?;
    }
    let mut sink = |block: &[f32]| -> Result<()> {
        if (data.len() + block.len()) as u64 / 2 > max_out {
            return Err(anyhow!("son trop long (max 30 min)"));
        }
        if data.capacity() - data.len() < block.len() {
            // Croissance d'un quart au moins : peu de recopies, peu de place perdue.
            let extra = block.len().max(data.capacity() / 4).max(1 << 16);
            data.try_reserve_exact(extra).map_err(oom)?;
        }
        data.extend_from_slice(block);
        Ok(())
    };
    let mut buf: Option<SampleBuffer<f32>> = None;
    let mut stereo: Vec<f32> = Vec::new();
    let mut read = 0u64;
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(SymError::ResetRequired) => break,
            Err(e) => return Err(anyhow!("lecture impossible : {e}")),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(SymError::DecodeError(_)) => continue,
            Err(e) => return Err(anyhow!("décodage impossible : {e}")),
        };
        let spec = *decoded.spec();
        let ch = spec.channels.count().max(1);
        let b = buf.get_or_insert_with(|| SampleBuffer::new(decoded.capacity() as u64, spec));
        if b.capacity() < decoded.capacity() * ch {
            *b = SampleBuffer::new(decoded.capacity() as u64, spec);
        }
        b.copy_interleaved_ref(decoded);
        let kept = trim.apply(b.samples(), ch);
        read += (kept.len() / ch) as u64;
        if read > max_src {
            return Err(anyhow!("son trop long (max 30 min)"));
        }
        // NaN / infini (fichier abîmé) : silence. Sans cela, un seul pad abîmé rendrait
        // tout le mixage de sortie NaN — donc muet — pendant qu'il joue.
        let clean = |x: f32| {
            if x.is_finite() {
                x.clamp(-16.0, 16.0)
            } else {
                0.0
            }
        };
        stereo.clear();
        for frame in kept.chunks_exact(ch) {
            stereo.push(clean(frame[0]));
            stereo.push(clean(if ch > 1 { frame[1] } else { frame[0] }));
        }
        conv.push(&stereo, 2, &mut sink)?;
        // Toujours appelé (longueur inconnue : 0) : c'est aussi le signe que le décodage
        // avance (processus fils, voir `isolate`).
        tick(n_frames.map_or(0.0, |n| read as f64 / n.max(1) as f64));
    }
    conv.finish(&mut sink)?;
    if data.is_empty() {
        return Err(anyhow!("fichier vide"));
    }
    Ok(data)
}

/// Le son reste en mémoire tout le spectacle : une place perdue notable est rendue (par
/// une copie faillible ; sans mémoire pour la faire, le son reste tel quel). Inutile dans
/// le processus fils : l'application reçoit le son dans une place exacte.
fn shrink(data: Vec<f32>) -> Vec<f32> {
    if data.capacity() - data.len() > data.len() / 16 + (1 << 16) {
        let mut exact: Vec<f32> = Vec::new();
        if exact.try_reserve_exact(data.len()).is_ok() {
            exact.extend_from_slice(&data);
            return exact;
        }
    }
    data
}

/// Chemin sur un partage réseau ou un périphérique (pas un simple fichier local) :
/// `\\serveur\partage\…`, `//serveur/…`, `\\?\UNC\…`, `\\?\GLOBALROOT\Device\Mup\…`,
/// `\\.\…`, ou un lecteur réseau (`Z:\…` relié à un partage). Seuls les chemins sur un
/// disque local (`C:\…`, `\\?\C:\…`) et les chemins relatifs sont considérés comme locaux.
pub fn is_network_path(p: &Path) -> bool {
    let s = p.to_string_lossy();
    let b = s.as_bytes();
    let sep = |c: u8| c == b'\\' || c == b'/';
    if b.len() >= 2 && sep(b[0]) && sep(b[1]) {
        // `\\?\C:\…` : chemin local écrit sous forme longue ; tout le reste (UNC, Mup,
        // GLOBALROOT, `\\.\`…) peut mener au réseau.
        let verbatim_disk = b.len() >= 6
            && &b[..4] == b"\\\\?\\"
            && b[4].is_ascii_alphabetic()
            && b[5] == b':'
            && (b.len() == 6 || b[6] == b'\\');
        return !verbatim_disk || drive_is_remote(b[4]);
    }
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        if let Some(Component::Prefix(px)) = p.components().next() {
            return match px.kind() {
                Prefix::Disk(d) | Prefix::VerbatimDisk(d) => drive_is_remote(d),
                _ => true,
            };
        }
    }
    false
}

/// Lecteur relié à un partage réseau (`net use Z: \\serveur\partage`).
#[cfg(windows)]
fn drive_is_remote(letter: u8) -> bool {
    use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
    const DRIVE_REMOTE: u32 = 4;
    let root = [
        letter.to_ascii_uppercase() as u16,
        b':' as u16,
        b'\\' as u16,
        0,
    ];
    // SAFETY : chaîne UTF-16 terminée par un zéro, lue seulement pendant l'appel.
    unsafe { GetDriveTypeW(root.as_ptr()) == DRIVE_REMOTE }
}

#[cfg(not(windows))]
fn drive_is_remote(_letter: u8) -> bool {
    false
}

/// Chemin relatif qui sort du dossier où il est écrit (`..`, racine, lecteur) : écrit par
/// quelqu'un d'autre que TracksPlus, qui n'enregistre que des chemins sous la session.
fn escapes_dir(p: &Path) -> bool {
    use std::path::Component;
    p.is_relative()
        && p.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
}

pub fn is_audio_file(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| AUDIO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// Copie `src` dans le dossier `Sons` de la session `dir` et renvoie le chemin de la copie.
/// Un son déjà dans ce dossier est gardé tel quel ; un fichier du même nom et au contenu
/// identique est réutilisé ; sinon le nom reçoit un suffixe « (2) », « (3) »…
pub fn copy_into_session(src: &Path, dir: &Path) -> Result<PathBuf> {
    let sounds = dir.join(SOUNDS_DIR);
    if src.parent() == Some(sounds.as_path()) && src.exists() {
        return Ok(src.to_path_buf());
    }
    let size = std::fs::metadata(src)
        .with_context(|| format!("Son introuvable : {}", src.display()))?
        .len();
    std::fs::create_dir_all(&sounds)?;
    let stem = src
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Son".into());
    let ext = src
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    for n in 1..1000 {
        let name = if n == 1 {
            format!("{stem}{ext}")
        } else {
            format!("{stem} ({n}){ext}")
        };
        let dst = sounds.join(name);
        match std::fs::metadata(&dst) {
            Ok(m) if m.len() == size && same_contents(src, &dst, size).unwrap_or(false) => {
                return Ok(dst);
            }
            Ok(_) => continue,
            Err(_) => {
                crate::project::copy_synced(src, &dst)?;
                return Ok(dst);
            }
        }
    }
    anyhow::bail!("Trop de sons nommés « {stem} » dans la session")
}

const EDGE_COMPARE: u64 = 1024 * 1024;

/// Deux fichiers de taille `size` ont le même contenu (en entier jusqu'à 64 Mo, sinon le
/// premier et le dernier Mo).
fn same_contents(a: &Path, b: &Path, size: u64) -> std::io::Result<bool> {
    // Comparaison complète : deux gros sons de même nom et même taille peuvent ne différer
    // qu'au milieu (début et fin silencieux). Les gros fichiers sont copiés en arrière-plan,
    // ce n'est donc jamais l'interface qui attend.
    compare_files(a, b, size, u64::MAX, EDGE_COMPARE)
}

fn compare_files(a: &Path, b: &Path, size: u64, full_max: u64, edge: u64) -> std::io::Result<bool> {
    use std::io::{Read, Seek, SeekFrom};
    let (mut fa, mut fb) = (std::fs::File::open(a)?, std::fs::File::open(b)?);
    let ranges: Vec<(u64, u64)> = if size <= full_max || size < 2 * edge {
        vec![(0, size)]
    } else {
        vec![(0, edge), (size - edge, edge)]
    };
    let (mut ba, mut bb) = (vec![0u8; 64 * 1024], vec![0u8; 64 * 1024]);
    for (start, len) in ranges {
        fa.seek(SeekFrom::Start(start))?;
        fb.seek(SeekFrom::Start(start))?;
        let mut left = len;
        while left > 0 {
            let n = left.min(ba.len() as u64) as usize;
            fa.read_exact(&mut ba[..n])?;
            fb.read_exact(&mut bb[..n])?;
            if ba[..n] != bb[..n] {
                return Ok(false);
            }
            left -= n as u64;
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tp-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn board_with_pad(dir: &Path) -> Board {
        let mut b = Board::default();
        b.set(0, Some(Pad::new(dir.join("kick.wav"), 3)));
        b
    }

    /// Soundboard illisible : erreur (jamais un soundboard vide sans message) ; le soundboard
    /// affiché à la place, protégé, n'écrase jamais le fichier tant qu'il n'est pas modifié,
    /// puis le met de côté (importable) avant d'écrire.
    #[test]
    fn unreadable_board_is_never_overwritten() {
        let dir = fresh_dir("sbguard");
        let path = dir.join(BOARD_FILE);
        std::fs::write(&path, b"{\"cols\": 6, \"pages\": [ abime").unwrap();
        assert!(Board::load_session(&dir).is_err());
        let mut shown = Board::default();
        shown.protect("soundboard illisible".into());
        assert!(!shown.changed_since_guard());
        // Réglages de l'ordinateur changés : ce n'est pas une modification du soundboard.
        shown.midi_port = Some("Launchpad".into());
        assert!(!shown.changed_since_guard());
        // Écriture refusée (fermeture, copie de sécurité…).
        assert!(shown.save_session(&dir).is_err());
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"{\"cols\": 6, \"pages\": [ abime"
        );
        // Vraie modification : l'ancien fichier est mis de côté, puis le nouveau écrit.
        shown.set(0, Some(Pad::new(dir.join("kick.wav"), 0)));
        assert!(shown.changed_since_guard());
        let aside = shown.release_guard(&dir).unwrap().unwrap();
        assert_eq!(
            std::fs::read(&aside).unwrap(),
            b"{\"cols\": 6, \"pages\": [ abime"
        );
        assert!(aside.extension().is_some_and(|e| e == "json"));
        shown.save_session(&dir).unwrap();
        assert!(Board::load_session(&dir).unwrap().has_pads());
        std::fs::remove_dir_all(dir).ok();
    }

    /// Soundboard d'une version plus récente : lu (valeurs inconnues → défaut, champs
    /// inconnus gardés), mais protégé ; un NaN écrit en `null` ne le rend pas illisible.
    #[test]
    fn newer_board_is_loaded_but_protected() {
        let dir = fresh_dir("sbnewer");
        let path = dir.join(BOARD_FILE);
        let mut b = board_with_pad(&dir);
        b.save_session(&dir).unwrap();
        let mut v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["format_version"], FORMAT_VERSION);
        // NaN sérialisé par serde_json : `null`.
        v["pages"][0]["pads"][0]["gain_db"] = serde_json::Value::Null;
        v["pages"][0]["pads"][0]["futur"] = "x".into();
        v["nouveau_reglage"] = 3.into();
        std::fs::write(&path, v.to_string()).unwrap();
        let back = Board::load_session(&dir).unwrap();
        assert!(
            back.guard.is_none(),
            "champs inconnus seuls : pas de protection"
        );
        let pad = back.pad(0).unwrap();
        assert_eq!((pad.gain_db, pad.color), (0.0, 3));
        assert_eq!(pad.extra["futur"], "x");
        back.save_session(&dir).unwrap();
        let again: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(again["nouveau_reglage"], 3);
        assert_eq!(again["pages"][0]["pads"][0]["futur"], "x");

        // Nouvelle variante de mode de pad, et pad illisible : lu, mais protégé.
        v["pages"][0]["pads"][0]["mode"] = "Stutter".into();
        v["pages"][0]["pads"][1] = serde_json::json!({ "pas": "un pad" });
        std::fs::write(&path, v.to_string()).unwrap();
        let back = Board::load_session(&dir).unwrap();
        assert!(back.guard.is_some());
        assert_eq!(back.pad(0).unwrap().mode, PadMode::Toggle);
        assert!(back.pad(1).is_none());
        assert!(back.save_session(&dir).is_err());
        // Numéro de format plus récent : protégé aussi.
        b.save_session(&dir).unwrap();
        let mut v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        v["format_version"] = 42.into();
        std::fs::write(&path, v.to_string()).unwrap();
        assert!(Board::load_session(&dir).unwrap().guard.is_some());
        b = Board::load(&path).unwrap();
        assert!(b.guard.is_none(), "import : pas de protection");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn sounds_are_copied_into_the_session() {
        let base = std::env::temp_dir().join(format!("tp-sbsess-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let ext = base.join("ailleurs");
        std::fs::create_dir_all(&ext).unwrap();
        let a = ext.join("kick.wav");
        std::fs::write(&a, b"aaaa").unwrap();
        let other = base.join("autre").join("kick.wav");
        std::fs::create_dir_all(other.parent().unwrap()).unwrap();
        std::fs::write(&other, b"bbbbbb").unwrap();
        let dir = base.join("Session");
        // Copie, réutilisation du même fichier, suffixe pour un homonyme différent.
        let c1 = copy_into_session(&a, &dir).unwrap();
        assert_eq!(c1, dir.join("Sons").join("kick.wav"));
        assert_eq!(copy_into_session(&a, &dir).unwrap(), c1);
        assert_eq!(copy_into_session(&c1, &dir).unwrap(), c1);
        let c2 = copy_into_session(&other, &dir).unwrap();
        assert_eq!(c2, dir.join("Sons").join("kick (2).wav"));
        // Même nom et même taille, mais un autre son : jamais confondu.
        let same_size = base.join("encore").join("kick.wav");
        std::fs::create_dir_all(same_size.parent().unwrap()).unwrap();
        std::fs::write(&same_size, b"aaab").unwrap();
        let c3 = copy_into_session(&same_size, &dir).unwrap();
        assert_eq!(c3, dir.join("Sons").join("kick (3).wav"));
        assert_eq!(std::fs::read(&c3).unwrap(), b"aaab");
        assert_eq!(copy_into_session(&same_size, &dir).unwrap(), c3);
        assert_eq!(std::fs::read(&c1).unwrap(), b"aaaa");
        // Enregistré en relatif, relu en absolu, et la session peut être déplacée.
        let mut b = Board::default();
        b.set(0, Some(Pad::new(c1.clone(), 0)));
        b.save_session(&dir).unwrap();
        let txt = std::fs::read_to_string(dir.join(BOARD_FILE)).unwrap();
        assert!(!txt.contains("ailleurs") && !txt.contains(&*base.to_string_lossy()));
        let moved = base.join("Session déplacée");
        std::fs::rename(&dir, &moved).unwrap();
        let r = Board::load_session(&moved).unwrap();
        assert_eq!(r.pad(0).unwrap().path, moved.join("Sons").join("kick.wav"));
        assert!(r.pad(0).unwrap().path.exists());
        // Session sans soundboard : vide.
        assert!(!Board::load_session(&base.join("vide")).unwrap().has_pads());
        // Adoption d'un soundboard importé : sons copiés, absents signalés.
        let mut imp = Board::default();
        imp.set(0, Some(Pad::new(a.clone(), 0)));
        imp.set(1, Some(Pad::new(base.join("absent.wav"), 0)));
        let target = base.join("Cible");
        let missing = imp.adopt_sounds(&target);
        assert_eq!(missing, vec![base.join("absent.wav")]);
        assert_eq!(
            imp.pad(0).unwrap().path,
            target.join("Sons").join("kick.wav")
        );
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn old_layouts_become_automatic() {
        for old in ["\"Free\"", "\"Controller\"", "\"Auto\""] {
            let l: PadLayout = serde_json::from_str(old).unwrap();
            assert_eq!(l, PadLayout::Auto, "{old}");
        }
        let l: PadLayout = serde_json::from_str("\"Manual\"").unwrap();
        assert_eq!(l, PadLayout::Manual);
        assert_eq!(Board::default().layout, PadLayout::Auto);
    }

    #[test]
    fn load_and_resample_wav() {
        let dir = std::env::temp_dir().join(format!("tp-sb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 44100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        for i in 0..44100 {
            w.write_sample(((i as f32 * 0.05).sin() * 16000.0) as i16)
                .unwrap();
        }
        w.finalize().unwrap();
        let s = load_sample(&path, 48000).unwrap();
        assert_eq!(s.frames, 48000);
        assert_eq!(s.data.len(), 96000);
        assert!(s.data[20000].abs() > 0.0 && s.data[20000] == s.data[20001]);

        let mut pad = Pad::new(path.clone(), 0);
        pad.start = 0.5;
        pad.fade_out = 10.0;
        let p = pad.params(&s);
        assert_eq!(p.start, 24000);
        assert_eq!(p.end, 48000);
        assert_eq!(p.fade_out, 24000);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn large_files_compare_edges() {
        let dir = std::env::temp_dir().join(format!("tp-cmp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a"), dir.join("b"));
        let mut data = vec![7u8; 3 * 1024 * 1024];
        std::fs::write(&a, &data).unwrap();
        let size = data.len() as u64;
        // Différence à la fin.
        *data.last_mut().unwrap() = 8;
        std::fs::write(&b, &data).unwrap();
        assert!(!same_contents(&a, &b, size).unwrap());
        assert!(!compare_files(&a, &b, size, 1024, 1024).unwrap());
        *data.last_mut().unwrap() = 7;
        // Différence au milieu : vue en comparaison complète, pas sur les seuls bords.
        data[size as usize / 2] = 9;
        std::fs::write(&b, &data).unwrap();
        assert!(!same_contents(&a, &b, size).unwrap());
        assert!(compare_files(&a, &b, size, 1024, 1024).unwrap());
        data[size as usize / 2] = 7;
        std::fs::write(&b, &data).unwrap();
        assert!(same_contents(&a, &b, size).unwrap());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn cache_decodes_in_pool_and_retains() {
        let dir = std::env::temp_dir().join(format!("tp-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut paths = Vec::new();
        for k in 0..5 {
            let p = dir.join(format!("s{k}.wav"));
            let mut w = hound::WavWriter::create(&p, spec).unwrap();
            for i in 0..1000 {
                w.write_sample(i as i16).unwrap();
                w.write_sample(-(i as i16)).unwrap();
            }
            w.finalize().unwrap();
            paths.push(p);
        }
        // Fichier abîmé : échec signalé, jamais « en chargement » pour toujours.
        let bad = dir.join("abime.wav");
        std::fs::write(&bad, b"RIFF....WAVEfmt n'importe quoi").unwrap();
        paths.push(bad.clone());
        let mut c = SampleCache::new(48000);
        for p in &paths {
            c.request(p, None);
        }
        let t0 = std::time::Instant::now();
        while c.map.values().any(|s| matches!(s, LoadState::Loading)) {
            c.poll();
            assert!(t0.elapsed() < std::time::Duration::from_secs(20));
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        for p in &paths[..5] {
            assert_eq!(c.get(p).unwrap().frames, 1000);
        }
        assert!(matches!(c.map.get(&bad), Some(LoadState::Failed(_))));
        let keep: std::collections::HashSet<PathBuf> = [paths[0].clone()].into();
        c.retain(&keep);
        assert_eq!(c.map.len(), 1);
        assert!(c.get(&paths[0]).is_some());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn board_roundtrip() {
        let mut b = Board::default();
        b.set(
            Board::slot(0, 3),
            Some(Pad::new("/x/Applause.wav".into(), 2)),
        );
        let dir = std::env::temp_dir().join(format!("tp-board-{}", std::process::id()));
        let path = dir.join("b.json");
        b.save(&path).unwrap();
        let c = Board::load(&path).unwrap();
        assert_eq!(c.pad(3).unwrap().name, "Applause");
        assert_eq!(c.all_pads().count(), 1);
        std::fs::remove_dir_all(dir).ok();
    }

    /// Pads sur un partage réseau désignés par un fichier de session : repérés au
    /// chargement, jamais copiés automatiquement ; les sons de la session restent normaux.
    #[test]
    fn network_pads_are_flagged_not_opened() {
        use std::path::Path;
        assert!(is_network_path(Path::new(r"\\attaquant\partage\a.wav")));
        assert!(is_network_path(Path::new("//serveur/partage/a.wav")));
        assert!(is_network_path(Path::new(r"\\?\UNC\serveur\p\a.wav")));
        // Autres formes qui passent par le redirecteur réseau (MUP).
        assert!(is_network_path(Path::new(
            r"\\?\GLOBALROOT\Device\Mup\h\p\a.wav"
        )));
        assert!(is_network_path(Path::new(
            r"\\?\GLOBALROOT\??\UNC\h\p\a.wav"
        )));
        assert!(is_network_path(Path::new(r"\\.\UNC\h\p\a.wav")));
        assert!(is_network_path(Path::new(r"\\.\C:\Sons\a.wav")));
        assert!(is_network_path(Path::new(r"/\serveur\p\a.wav")));
        assert!(is_network_path(Path::new(r"\\?\Volume{0}\a.wav")));
        assert!(!is_network_path(Path::new(r"\\?\C:\Sons\a.wav")));
        assert!(!is_network_path(Path::new(r"C:\Sons\a.wav")));
        assert!(!is_network_path(Path::new(r"Sons\a.wav")));
        // Chemins relatifs qui sortent de la session : mis de côté eux aussi.
        assert!(escapes_dir(Path::new(r"..\..\..\.ssh\id_ed25519")));
        assert!(escapes_dir(Path::new("Sons/../../x.wav")));
        assert!(!escapes_dir(Path::new("Sons/b.wav")));
        assert!(!escapes_dir(Path::new(r"C:\Sons\a.wav")));
        let dir = std::env::temp_dir().join(format!("tp-unc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut b = Board::default();
        b.set(0, Some(Pad::new(PathBuf::from(r"\\attaquant\p\a.wav"), 0)));
        b.set(1, Some(Pad::new(PathBuf::from("Sons/b.wav"), 0)));
        b.save(&dir.join(BOARD_FILE)).unwrap();
        let mut l = Board::load_session(&dir).unwrap();
        assert_eq!(l.network_pads, vec![PathBuf::from(r"\\attaquant\p\a.wav")]);
        assert_eq!(l.pad(1).unwrap().path, dir.join("Sons/b.wav"));
        let missing = l.adopt_sounds(&dir.join("copie"));
        assert!(missing.contains(&PathBuf::from(r"\\attaquant\p\a.wav")));
        std::fs::remove_dir_all(dir).ok();
    }

    /// Session reçue dont un pad désigne un fichier hors de la session (`..`) ou un fichier
    /// qui n'est pas un son : jamais copié dans la session par « Enregistrer sous ».
    #[test]
    fn pads_outside_the_session_are_never_copied() {
        let base = std::env::temp_dir().join(format!("tp-exfil-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let dir = base.join("Session");
        std::fs::create_dir_all(dir.join("Sons")).unwrap();
        let secret = base.join("secret");
        std::fs::write(&secret, b"cle privee").unwrap();
        let doc = base.join("notes.txt");
        std::fs::write(&doc, b"document").unwrap();
        let mut b = Board::default();
        b.set(0, Some(Pad::new(PathBuf::from(r"..\secret"), 0)));
        b.set(1, Some(Pad::new(doc.clone(), 0)));
        b.save(&dir.join(BOARD_FILE)).unwrap();
        let mut l = Board::load_session(&dir).unwrap();
        assert_eq!(l.network_pads, vec![PathBuf::from(r"..\secret")]);
        let target = base.join("Copie");
        let missing = l.adopt_sounds(&target);
        assert_eq!(missing.len(), 2);
        assert!(!target.join("Sons").exists());
        // Le cache ne l'ouvre jamais, même après un changement de fréquence.
        let mut c = SampleCache::new(48000);
        c.park(&PathBuf::from(r"..\secret"));
        c.set_sample_rate(44100);
        c.reload(&PathBuf::from(r"..\secret"));
        assert!(matches!(
            c.map.get(&PathBuf::from(r"..\secret")),
            Some(LoadState::Failed(_))
        ));
        let _ = std::fs::remove_dir_all(base);
    }

    fn write_tone(path: &Path, sr: u32, frames: usize) {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: sr,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for i in 0..frames {
            let v = ((i as f32 * 0.01).sin() * 8000.0) as i16;
            w.write_sample(v).unwrap();
            w.write_sample(v).unwrap();
        }
        w.finalize().unwrap();
    }

    fn wait_loaded(c: &mut SampleCache) {
        let t0 = std::time::Instant::now();
        loop {
            c.poll();
            if !c.map.values().any(|s| matches!(s, LoadState::Loading)) {
                break;
            }
            assert!(t0.elapsed() < std::time::Duration::from_secs(20));
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// Mémoire réservée aux sons : au-delà, le pad affiche une erreur au lieu de décoder
    /// (et d'allouer) un son de plus.
    #[test]
    fn memory_budget_refuses_extra_sounds() {
        let dir = std::env::temp_dir().join(format!("tp-budget-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.wav"), dir.join("b.wav"));
        write_tone(&a, 48000, 48000);
        write_tone(&b, 48000, 48000);
        // Place pour un son d'une seconde (≈ 390 Ko), pas pour deux.
        let mut c = SampleCache::with_budget(48000, 600_000);
        c.request(&a, None);
        wait_loaded(&mut c);
        assert!(c.get(&a).is_some());
        c.request(&b, None);
        wait_loaded(&mut c);
        match c.map.get(&b) {
            Some(LoadState::Failed(e)) => assert!(e.contains("Mémoire"), "{e}"),
            _ => panic!("second son accepté malgré la limite"),
        }
        // Le premier retiré : la place est rendue, le second se charge.
        c.retain(&[b.clone()].into());
        c.reload(&b);
        wait_loaded(&mut c);
        assert!(c.get(&b).is_some());
        assert_eq!(c.budget.inflight.load(Ordering::Relaxed), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Plusieurs sons refusés faute de place pendant que des sons à l'ancienne fréquence
    /// sont gardés de côté : tous sont relancés une fois la place rendue (ceux du même lot
    /// comme ceux qui arrivent ensuite), pas seulement le premier.
    #[test]
    fn memory_full_with_stash_retries_every_refusal() {
        let dir = std::env::temp_dir().join(format!("tp-stash-full-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let paths: Vec<PathBuf> = (0..3).map(|i| dir.join(format!("{i}.wav"))).collect();
        for p in &paths {
            write_tone(p, 48000, 4800);
        }
        let mut c = SampleCache::with_budget(48000, 4_000_000);
        let old = SampleData {
            sample_rate: 44100,
            frames: 200_000,
            data: vec![0.0; 400_000],
            peaks: Vec::new(),
        };
        let old_map = HashMap::from([(dir.join("ancien.wav"), Arc::new(old))]);
        c.stash = Some((44100, old_map, std::time::Instant::now()));
        c.recount();
        // Refus décidés par les threads de décodage pendant que ces sons comptaient.
        let refuse = |c: &mut SampleCache, p: &PathBuf, generation: u64| {
            c.tx.send(Decoded {
                path: p.clone(),
                generation,
                result: Err(MEMORY_FULL.into()),
                reserved: 0,
                full: true,
            })
            .unwrap();
        };
        let mut gens = Vec::new();
        for p in &paths {
            c.map.insert(p.clone(), LoadState::Loading);
            c.next_gen += 1;
            c.pending.lock().insert(p.clone(), c.next_gen);
            gens.push(c.next_gen);
        }
        // Deux refus dans le même lot…
        refuse(&mut c, &paths[0], gens[0]);
        refuse(&mut c, &paths[1], gens[1]);
        c.poll();
        assert!(c.stash.is_none());
        // … et un troisième qui arrive après la libération.
        refuse(&mut c, &paths[2], gens[2]);
        wait_loaded(&mut c);
        for p in &paths {
            assert!(c.get(p).is_some(), "{} resté refusé", p.display());
        }
        assert_eq!(c.budget.inflight.load(Ordering::Relaxed), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Réservation faite sur la taille annoncée, puis ajustée à la taille réelle :
    /// réduite toujours, agrandie seulement s'il reste de la place.
    #[test]
    fn budget_reservation_is_resized() {
        let b = Budget::new(1000);
        assert!(b.resize(0, 400));
        assert!(b.resize(400, 900));
        assert!(!b.resize(900, 1100));
        assert_eq!(b.inflight.load(Ordering::Relaxed), 900);
        assert!(b.resize(900, 100));
        assert_eq!(b.available(), 900);
        b.committed.store(950, Ordering::Relaxed);
        assert_eq!(b.available(), 0);
    }

    /// Reconnexion qui revient à la fréquence d'avant : sons repris sans redécodage.
    #[test]
    fn rate_flip_back_reuses_previous_sounds() {
        let dir = std::env::temp_dir().join(format!("tp-flip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.wav");
        write_tone(&a, 48000, 4800);
        let mut c = SampleCache::new(48000);
        c.request(&a, None);
        wait_loaded(&mut c);
        let before = c.get(&a).unwrap().clone();
        c.set_sample_rate(44100);
        wait_loaded(&mut c);
        assert_eq!(c.get(&a).unwrap().sample_rate, 44100);
        c.set_sample_rate(48000);
        // Aucune attente : le son à 48 kHz est repris tel quel.
        assert!(Arc::ptr_eq(c.get(&a).unwrap(), &before));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// MP3 encodé par LAME : délai d'encodeur et bourrage retirés (lecture « gapless »),
    /// le son décodé a la longueur de l'original — un pad en boucle n'a pas de trou.
    #[test]
    fn mp3_is_decoded_gapless() {
        use mp3lame_encoder::{Builder, FlushGap, InterleavedPcm};
        let dir = std::env::temp_dir().join(format!("tp-gapless-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let frames = 44100 + 333;
        let pcm: Vec<f32> = (0..frames)
            .flat_map(|i| {
                let v = (i as f32 * 0.02).sin() * 0.5;
                [v, v]
            })
            .collect();
        let mut b = Builder::new().unwrap();
        b.set_num_channels(2).unwrap();
        b.set_sample_rate(44100).unwrap();
        b.set_to_write_vbr_tag(true).unwrap();
        let mut enc = b.build().unwrap();
        let mut out = Vec::with_capacity(frames * 5 / 4 + 7200);
        enc.encode_to_vec(InterleavedPcm(&pcm), &mut out).unwrap();
        out.reserve(7200);
        enc.flush_to_vec::<FlushGap>(&mut out).unwrap();
        // Trame LAME/Xing réécrite au début (délai et bourrage).
        let mut tag = Vec::with_capacity(enc.lame_tag_size());
        assert!(enc.lame_tag_encode_to_vec(&mut tag).is_some());
        out[..tag.len()].copy_from_slice(&tag);
        let p = dir.join("boucle.mp3");
        std::fs::write(&p, &out).unwrap();
        let s = load_sample(&p, 44100).unwrap();
        assert!(
            (s.frames as i64 - frames as i64).abs() <= 1,
            "{} trames décodées pour {frames}",
            s.frames
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
