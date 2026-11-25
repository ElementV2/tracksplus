//! Moteur audio : choix de la carte son, flux d'entrée (enregistrement + vumètres)
//! et flux de sortie (lecture).
//!
//! Les callbacks audio ne font aucune allocation ni aucun verrou : ils échangent les
//! données avec les threads disque via des ring buffers `rtrb`.

use crate::sampler::{MAX_PADS, PadCmd, SampleData, Sampler};
use anyhow::{Context, Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};
use crossbeam_channel::{Receiver, Sender, bounded};
use parking_lot::Mutex;
use rtrb::{Consumer, Producer};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

/// Nombre maximum de canaux d'entrée suivis par les vumètres.
pub const MAX_INPUTS: usize = 256;

pub const COMMON_RATES: [u32; 6] = [44100, 48000, 88200, 96000, 176400, 192000];
pub const BUFFER_SIZES: [u32; 7] = [32, 64, 128, 256, 512, 1024, 2048];

/// Réglages audio mémorisés entre deux lancements.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AudioPrefs {
    pub host: Option<String>,
    pub input: Option<String>,
    pub output: Option<String>,
    pub sample_rate: Option<u32>,
    pub buffer: Option<u32>,
    /// Premier canal de sortie (0-based) de la paire stéréo d'écoute.
    #[serde(default)]
    pub out_left: u16,
    /// Sortie du soundboard (`None` = même périphérique que l'écoute multipiste).
    #[serde(default)]
    pub pad_output: Option<String>,
    /// Premier canal de la paire du soundboard.
    #[serde(default)]
    pub pad_left: u16,
    /// Sortie du monitoring direct des entrées (`None` = même périphérique que l'écoute).
    #[serde(default)]
    pub monitor_output: Option<String>,
    /// Premier canal de la paire de monitoring.
    #[serde(default)]
    pub monitor_left: u16,
    /// Prise arrêtée par une perte de la carte : relancée toute seule dès son retour, à la
    /// bonne position sur la timeline (activé par défaut).
    #[serde(default = "yes")]
    pub auto_restart: bool,
}

fn yes() -> bool {
    true
}

impl Default for AudioPrefs {
    fn default() -> Self {
        Self {
            host: None,
            input: None,
            output: None,
            sample_rate: None,
            buffer: None,
            out_left: 0,
            pad_output: None,
            pad_left: 0,
            monitor_output: None,
            monitor_left: 0,
            auto_restart: true,
        }
    }
}

impl AudioPrefs {
    /// Sortie du soundboard pour le moteur.
    /// « Comme le main multipiste » (`None`) : même périphérique et même paire que l'écoute.
    pub fn pad_out(&self) -> PadOut {
        PadOut {
            device: self.pad_output.clone(),
            left: if self.pad_output.is_some() {
                self.pad_left
            } else {
                self.out_left
            },
        }
    }

    /// Paire de monitoring ; « Comme le main multipiste » suit la paire de l'écoute.
    pub fn monitor_pair(&self) -> u16 {
        if self.monitor_output.is_some() {
            self.monitor_left
        } else {
            self.out_left
        }
    }

    fn path() -> Option<std::path::PathBuf> {
        dirs::config_dir().map(|d| d.join("TracksPlus").join("audio.json"))
    }

    pub fn load() -> Self {
        Self::path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        if let Some(p) = Self::path() {
            if let Some(d) = p.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            if let Ok(s) = serde_json::to_string_pretty(self) {
                // Écriture protégée : une coupure ne laisse jamais un fichier tronqué.
                let _ = crate::project::write_atomic(&p, s.as_bytes());
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub name: String,
    pub channels: u16,
    /// Fréquences acceptées par le pilote (en WASAPI partagé : seulement celle réglée
    /// dans Windows ; en ASIO : celles de la carte).
    pub rates: Vec<u32>,
    /// Fréquence utilisée quand on laisse « Auto ».
    pub default_rate: Option<u32>,
    /// Tailles de buffer acceptées (trames), si le pilote les indique.
    pub buffer: Option<(u32, u32)>,
}

pub fn host_names() -> Vec<String> {
    cpal::available_hosts()
        .into_iter()
        .map(|h| h.name().to_string())
        .collect()
}

fn find_host(name: Option<&str>) -> cpal::Host {
    if let Some(name) = name {
        for id in cpal::available_hosts() {
            if id.name() == name
                && let Ok(h) = cpal::host_from_id(id)
            {
                return h;
            }
        }
    }
    cpal::default_host()
}

/// Noms des périphériques d'entrée et de sortie par défaut d'un pilote.
pub fn default_device_names(host: Option<&str>) -> (Option<String>, Option<String>) {
    let h = find_host(host);
    (
        h.default_input_device().map(|d| d.to_string()),
        h.default_output_device().map(|d| d.to_string()),
    )
}

pub fn default_host_name() -> String {
    cpal::default_host().id().name().to_string()
}

fn ranges_info(
    name: String,
    ranges: impl Iterator<Item = cpal::SupportedStreamConfigRange>,
    default_rate: Option<u32>,
) -> DeviceInfo {
    let mut channels = 0;
    let mut rates = Vec::new();
    let mut buffer: Option<(u32, u32)> = None;
    for r in ranges {
        channels = channels.max(r.channels());
        if let cpal::SupportedBufferSize::Range { min, max } = *r.buffer_size() {
            buffer = Some(buffer.map_or((min, max), |(a, b)| (a.min(min), b.max(max))));
        }
        for &sr in &COMMON_RATES {
            if r.min_sample_rate() <= sr && sr <= r.max_sample_rate() && !rates.contains(&sr) {
                rates.push(sr);
            }
        }
    }
    rates.sort();
    DeviceInfo {
        name,
        channels,
        rates,
        default_rate,
        buffer,
    }
}

/// Liste les périphériques d'entrée et de sortie d'un pilote.
pub fn list_devices(host: Option<&str>) -> (Vec<DeviceInfo>, Vec<DeviceInfo>) {
    let host = find_host(host);
    let mut ins = Vec::new();
    let mut outs = Vec::new();
    if let Ok(devs) = host.devices() {
        for d in devs {
            let name = d.to_string();
            if let Ok(cfgs) = d.supported_input_configs() {
                let sr = d.default_input_config().ok().map(|c| c.sample_rate());
                let info = ranges_info(name.clone(), cfgs, sr);
                if info.channels > 0 {
                    ins.push(info);
                }
            }
            if let Ok(cfgs) = d.supported_output_configs() {
                let sr = d.default_output_config().ok().map(|c| c.sample_rate());
                let info = ranges_info(name, cfgs, sr);
                if info.channels > 0 {
                    outs.push(info);
                }
            }
        }
    }
    (ins, outs)
}

/// État partagé entre l'interface et les callbacks audio (atomiques uniquement).
pub struct Shared {
    /// Crête par canal d'entrée depuis la dernière lecture (bits f32).
    pub meters: Vec<AtomicU32>,
    /// Trames enregistrées depuis le début de la prise.
    pub rec_frames: AtomicU64,
    /// Trames jouées depuis le début de la lecture.
    pub out_frames: AtomicU64,
    /// Position de lecture (trames depuis son début) au moment où la prise a vraiment
    /// commencé dans le callback d'entrée : sert à poser une prise faite en écoutant.
    pub rec_play_offset: AtomicU64,
    /// Début du dernier bloc de lecture demandé par le pilote : valeur de `out_frames`
    /// et instant (ns depuis `epoch`, 0 = pas de lecture). Permet de situer la lecture à
    /// l'échantillon près entre deux callbacks de sortie (prises faites en écoutant).
    pub out_cb_frames: AtomicU64,
    pub out_cb_ns: AtomicU64,
    epoch: std::time::Instant,
    /// Données perdues car le disque n'a pas suivi.
    pub overruns: AtomicU32,
    /// Idem pour la copie de sécurité (second disque).
    pub copy_overruns: AtomicU32,
    /// Trous de lecture.
    pub underruns: AtomicU32,
    /// Taille réelle des blocs reçus du pilote (trames par appel), entrée et sortie :
    /// c'est le buffer effectivement utilisé, même en « Auto ».
    pub in_block: AtomicU32,
    pub out_block: AtomicU32,
    /// Xruns signalés par le pilote, comptés seulement pendant une prise ou une lecture.
    pub xruns: AtomicU32,
    /// Prise ou lecture en cours : les décrochages du pilote comptent. À l'arrêt, ils
    /// n'ont aucune conséquence (et certains pilotes, surtout virtuels, en signalent).
    pub counting: AtomicBool,
    pub last_error: Mutex<Option<String>>,
    /// Dernière erreur d'un flux (code de `error_code`, 0 = aucune).
    pub stream_error: std::sync::atomic::AtomicU8,
    /// Flux disparus ou invalidés (DVS relancé, USB débranché, fréquence modifiée dans
    /// le pilote), un bit par flux (`LOST_*`) : l'interface les rouvre.
    pub device_lost: std::sync::atomic::AtomicU8,
    /// Crêtes L/R de la lecture multipiste (bits f32).
    pub master_play: [AtomicU32; 2],
    /// Crêtes L/R du soundboard (bits f32).
    pub master_pad: [AtomicU32; 2],
    /// Crêtes L/R du monitoring direct des entrées (bits f32).
    pub master_mon: [AtomicU32; 2],
    /// Monitoring direct, par canal d'entrée : 0 = non écouté, 1 = au centre (piste mono),
    /// 2 = à gauche, 3 = à droite (piste stéréo).
    pub monitor: Vec<std::sync::atomic::AtomicU8>,
    /// Au moins un canal est écouté (évite tout calcul sinon).
    pub monitor_on: AtomicBool,
    /// Position de lecture de chaque pad (+1, 0 = arrêté).
    pub pad_pos: Vec<AtomicU64>,
    /// Latence capture → callback d'entrée, d'après le pilote (trames, 0 = inconnue).
    pub in_latency: AtomicU32,
    /// Latence callback de sortie → restitution, d'après le pilote (trames, 0 = inconnue).
    pub out_latency: AtomicU32,
    /// Battement de cœur de chaque flux (`SLOT_*`) : nombre d'appels du callback. Un
    /// compteur qui n'avance plus = flux arrêté sans erreur du pilote (horloge Dante perdue,
    /// pilote bloqué) : l'interface le rouvre.
    pub heartbeat: [AtomicU64; SLOTS],
    /// Silence inséré dans la prise en cours à la place d'un trou de l'entrée (trames) :
    /// la suite reste à sa place sur la timeline.
    pub rec_gap_frames: AtomicU64,
}

impl Shared {
    fn new() -> Self {
        Self {
            meters: (0..MAX_INPUTS).map(|_| AtomicU32::new(0)).collect(),
            rec_frames: AtomicU64::new(0),
            out_frames: AtomicU64::new(0),
            rec_play_offset: AtomicU64::new(0),
            out_cb_frames: AtomicU64::new(0),
            out_cb_ns: AtomicU64::new(0),
            epoch: std::time::Instant::now(),
            overruns: AtomicU32::new(0),
            copy_overruns: AtomicU32::new(0),
            underruns: AtomicU32::new(0),
            xruns: AtomicU32::new(0),
            counting: AtomicBool::new(false),
            in_block: AtomicU32::new(0),
            out_block: AtomicU32::new(0),
            last_error: Mutex::new(None),
            stream_error: std::sync::atomic::AtomicU8::new(0),
            device_lost: std::sync::atomic::AtomicU8::new(0),
            master_play: [AtomicU32::new(0), AtomicU32::new(0)],
            master_pad: [AtomicU32::new(0), AtomicU32::new(0)],
            master_mon: [AtomicU32::new(0), AtomicU32::new(0)],
            monitor: (0..MAX_INPUTS)
                .map(|_| std::sync::atomic::AtomicU8::new(0))
                .collect(),
            monitor_on: AtomicBool::new(false),
            pad_pos: (0..MAX_PADS).map(|_| AtomicU64::new(0)).collect(),
            in_latency: AtomicU32::new(0),
            out_latency: AtomicU32::new(0),
            heartbeat: std::array::from_fn(|_| AtomicU64::new(0)),
            rec_gap_frames: AtomicU64::new(0),
        }
    }

    /// Panique rattrapée dans un callback : le flux se tait et sera rouvert.
    fn fault(&self, stream: u8) {
        self.device_lost.fetch_or(stream, Ordering::Release);
        self.stream_error.store(8, Ordering::Release);
    }

    /// Latence aller-retour signalée par le pilote (entrée + sortie, en trames ;
    /// 0 = inconnue). Ne comprend pas la latence propre des convertisseurs.
    pub fn round_trip_latency(&self) -> u32 {
        self.in_latency
            .load(Ordering::Relaxed)
            .saturating_add(self.out_latency.load(Ordering::Relaxed))
    }

    pub fn take_peak(a: &AtomicU32) -> f32 {
        f32::from_bits(a.swap(0, Ordering::Relaxed))
    }

    /// Lit puis remet à zéro la crête d'un canal.
    pub fn take_meter(&self, ch: usize) -> f32 {
        self.meters
            .get(ch)
            .map(|m| f32::from_bits(m.swap(0, Ordering::Relaxed)))
            .unwrap_or(0.0)
    }

    fn now_ns(&self) -> u64 {
        // Jamais 0 (réservé à « pas de lecture »).
        (self.epoch.elapsed().as_nanos() as u64).max(1)
    }

    /// Position de lecture (trames) à l'instant présent, interpolée depuis le début du
    /// dernier bloc de sortie, sans jamais dépasser ce qui a déjà été demandé.
    fn play_position_now(&self, sample_rate: u32) -> u64 {
        let ns = self.out_cb_ns.load(Ordering::Acquire);
        let base = self.out_cb_frames.load(Ordering::Relaxed);
        let cur = self.out_frames.load(Ordering::Relaxed);
        if ns == 0 || cur < base {
            return cur;
        }
        let dt = self.now_ns().saturating_sub(ns);
        let ahead = (dt as u128 * sample_rate as u128 / 1_000_000_000) as u64;
        (base + ahead).min(cur)
    }

    /// `stream` : bit `LOST_*` du flux qui signale l'erreur.
    fn on_error(&self, err: cpal::Error, stream: u8) {
        // Erreurs après lesquelles le flux est mort (cpal sort de sa boucle) : carte
        // débranchée, flux invalidé, service audio arrêté, carte prise par une autre
        // application. « DeviceChanged » n'est pas une perte (le flux continue, selon cpal),
        // ni « BackendError » : cpal l'émet aussi pour des échecs sans conséquence (priorité
        // temps réel refusée) — un flux vraiment mort est repéré par son battement de cœur.
        if matches!(
            err.kind(),
            cpal::ErrorKind::DeviceNotAvailable
                | cpal::ErrorKind::StreamInvalidated
                | cpal::ErrorKind::HostUnavailable
                | cpal::ErrorKind::DeviceBusy
        ) {
            self.device_lost.fetch_or(stream, Ordering::Release);
        }
        if err.kind() == cpal::ErrorKind::Xrun {
            if self.counting.load(Ordering::Relaxed) {
                self.xruns.fetch_add(1, Ordering::Relaxed);
            }
        } else if err.kind() == cpal::ErrorKind::DeviceChanged {
            // Périphérique par défaut changé : le flux continue, rien à signaler.
        } else {
            // Rien à allouer ni à verrouiller ici (ce rappel peut tourner sur le thread
            // audio) : juste le genre d'erreur, l'interface écrit le message.
            self.stream_error
                .store(error_code(err.kind()), Ordering::Release);
        }
    }

    /// Dernière erreur à afficher : ouverture de la carte, sinon erreur d'un flux.
    pub fn error_text(&self) -> Option<String> {
        if let Some(e) = self.last_error.lock().clone() {
            return Some(e);
        }
        let msg = match self.stream_error.load(Ordering::Acquire) {
            0 => return None,
            1 => "la carte son n'est plus disponible (débranchée ou pilote arrêté)",
            2 => "flux audio interrompu par le pilote (réglage changé, pilote relancé)",
            3 => "carte son occupée par une autre application",
            4 => "réglage refusé par la carte son",
            5 => "accès à la carte son refusé par le système",
            6 => "ressources du pilote épuisées",
            8 => "erreur interne du traitement audio : flux rouvert automatiquement",
            _ => "erreur du pilote audio",
        };
        Some(msg.to_string())
    }
}

/// Code (sans allocation) d'une erreur de flux, voir `Shared::error_text`.
fn error_code(k: cpal::ErrorKind) -> u8 {
    use cpal::ErrorKind as K;
    match k {
        K::DeviceNotAvailable => 1,
        K::StreamInvalidated => 2,
        K::DeviceBusy => 3,
        K::UnsupportedConfig | K::InvalidInput => 4,
        K::PermissionDenied | K::RealtimeDenied => 5,
        K::ResourceExhausted => 6,
        _ => 7,
    }
}

/// Flux concernés par une perte de périphérique (`Shared::device_lost`), un bit par flux
/// (`SLOT_*`) : entrée, sortie principale (lecture multipiste), sortie à part du soundboard,
/// sortie à part du monitoring.
pub const LOST_INPUT: u8 = 1;
pub const LOST_OUTPUT: u8 = 2;
pub const LOST_PADS: u8 = 4;
pub const LOST_MONITOR: u8 = 8;
pub const LOST_ALL: u8 = LOST_INPUT | LOST_OUTPUT | LOST_PADS | LOST_MONITOR;

/// Flux du moteur (indices de `Shared::heartbeat`, bit `slot_bit(SLOT_*)` = `LOST_*`).
pub const SLOT_INPUT: usize = 0;
pub const SLOT_MAIN: usize = 1;
pub const SLOT_PADS: usize = 2;
pub const SLOT_MON: usize = 3;
pub const SLOTS: usize = 4;

pub const fn slot_bit(slot: usize) -> u8 {
    1 << slot
}

/// Délai maximal d'activation d'un flux par le pilote : une ouverture ne reste jamais
/// bloquée indéfiniment (service audio figé après une mise en veille).
const OPEN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Trou de l'entrée au-delà duquel du silence est inséré dans la prise (s) : en dessous,
/// c'est le grain de l'horloge du pilote, pas une coupure.
const GAP_FILL_MIN: f64 = 0.25;
/// Silence inséré au plus pour un seul trou (s).
const GAP_FILL_MAX: f64 = 60.0;

/// Durée → trames à la fréquence `sr` (saturé, sans allocation).
fn to_frames(d: std::time::Duration, sr: u32) -> u32 {
    (d.as_secs_f64() * sr as f64).min(u32::MAX as f64) as u32
}

/// Une entrée à enregistrer : canal (ou paire stéréo) de la carte → ring buffer vers le
/// disque. En stéréo, les échantillons sont entrelacés (G, D, G, D…).
pub struct RecTarget {
    /// Canaux de la carte (le second n'est utilisé qu'en stéréo).
    pub channels: [usize; 2],
    /// 1 = mono, 2 = stéréo.
    pub width: usize,
    pub prod: Producer<f32>,
    /// Trames perdues (tampon plein) pas encore compensées par du silence.
    pub pending: usize,
    /// Copie de sécurité (second disque) : son propre tampon, rempli ici même — elle ne
    /// dépend en rien du disque principal. (tampon, trames perdues à compenser)
    pub copy: Option<(Producer<f32>, usize)>,
}

impl RecTarget {
    pub fn new(channel: usize, prod: Producer<f32>) -> Self {
        Self {
            channels: [channel, channel],
            width: 1,
            prod,
            pending: 0,
            copy: None,
        }
    }

    /// Avec copie de sécurité vers `copy`.
    pub fn with_copy(mut self, copy: Option<Producer<f32>>) -> Self {
        self.copy = copy.map(|p| (p, 0));
        self
    }

    /// Paire stéréo `left`, `left + 1`.
    pub fn stereo(left: usize, prod: Producer<f32>) -> Self {
        Self {
            channels: [left, left + 1],
            width: 2,
            prod,
            pending: 0,
            copy: None,
        }
    }
}

enum InCmd {
    Record(Vec<RecTarget>),
    Stop,
    /// Nouveau tampon de monitoring (la sortie qui le joue a été rouverte).
    Monitor(Producer<f32>),
}

/// Rendu par le callback d'entrée pour être libéré hors du thread audio (le contenu n'est
/// jamais lu : il est seulement libéré ici).
#[allow(dead_code)]
enum InGarbage {
    Targets(Vec<RecTarget>),
    Monitor(Producer<f32>),
}

enum OutCmd {
    Play(Consumer<f32>),
    /// Même lecture, re-rendue (mute / solo changés) : le nouveau tampon commence à la
    /// trame `at` de la lecture en cours et prend le relais à ce moment précis.
    Switch(Consumer<f32>, u64),
    Stop,
    /// Nouveau tampon de monitoring (l'entrée a été rouverte), préparé hors du thread audio.
    Monitor(MonitorOut),
}

/// Rendu par un callback de sortie pour être libéré hors du thread audio (le contenu n'est
/// jamais lu : il est seulement libéré ici).
#[allow(dead_code)]
enum OutGarbage {
    Play(Consumer<f32>),
    Monitor(MonitorOut),
}

#[derive(Clone, Debug, Default)]
pub struct EngineStatus {
    pub host: String,
    pub input: Option<String>,
    pub output: Option<String>,
    pub sample_rate: u32,
    pub in_channels: u16,
    pub out_channels: u16,
    /// Sortie du soundboard quand elle est sur un autre périphérique que l'écoute.
    pub pad_output: Option<String>,
    /// Canaux du périphérique où joue le soundboard.
    pub pad_out_channels: u16,
    /// Sortie du monitoring quand elle est sur un périphérique à part.
    pub monitor_output: Option<String>,
    /// Le monitoring a bien une sortie active.
    pub monitor_ok: bool,
    pub buffer: Option<u32>,
    pub in_format: Option<SampleFormat>,
}

/// Où joue le soundboard : `device` = périphérique de sortie (`None` = le même que
/// l'écoute multipiste), `left` = premier canal de la paire stéréo.
#[derive(Clone, Debug, Default)]
pub struct PadOut {
    pub device: Option<String>,
    pub left: u16,
}

/// Périphérique voulu pour un flux.
#[derive(Clone, Debug, Default, PartialEq)]
struct Want {
    /// Nom choisi. `None` = défaut du système, figé sur un périphérique précis dès la
    /// première ouverture : un casque Bluetooth ou une webcam qui devient le défaut de
    /// Windows en pleine prise ne remplace jamais la carte.
    name: Option<String>,
    /// Identifiant stable du pilote, connu dès la première ouverture : la carte est
    /// retrouvée par lui d'abord, puis par son nom.
    id: Option<String>,
}

impl Want {
    fn named(name: Option<String>) -> Self {
        Self { name, id: None }
    }

    /// Périphérique connu (choisi, ou défaut déjà résolu) : son flux est attendu.
    fn wanted(&self) -> bool {
        self.name.is_some() || self.id.is_some()
    }

    fn label(&self) -> String {
        self.name.clone().unwrap_or_else(|| "par défaut".into())
    }
}

/// Périphérique trouvé par `find_device`.
#[derive(Clone)]
struct Found {
    dev: cpal::Device,
    name: String,
    id: Option<String>,
    /// Retrouvé sous un autre nom que celui voulu (préfixe « 2- » ajouté par Windows).
    renamed: bool,
}

/// Ce que le moteur ouvre : réglages résolus à la première ouverture, réutilisés tels quels
/// pour rouvrir un flux perdu (même fréquence, mêmes périphériques).
#[derive(Clone)]
struct Plan {
    host: Option<String>,
    host_name: String,
    sample_rate: u32,
    buffer: Option<u32>,
    /// Périphérique de chaque flux (`SLOT_*`) ; soundboard et monitoring seulement s'ils
    /// ont leur propre flux.
    want: [Want; SLOTS],
    /// Flux qui joue le monitoring : `SLOT_MAIN`, `SLOT_PADS` ou `SLOT_MON`.
    mon_on: usize,
    /// Flux réglés sur « Par défaut », figés sur le périphérique par défaut d'alors.
    defaulted: u8,
    out_left: u16,
    pad_left: u16,
    mon_left: u16,
}

impl Plan {
    /// Flux attendus (bits `LOST_*`).
    fn wanted(&self) -> u8 {
        (0..SLOTS)
            .filter(|&s| self.want[s].wanted())
            .fold(0, |m, s| m | slot_bit(s))
    }

    fn buffer_size(&self) -> cpal::BufferSize {
        match self.buffer {
            Some(b) => cpal::BufferSize::Fixed(b),
            None => cpal::BufferSize::Default,
        }
    }
}

/// Nom d'un flux en tête de message.
fn slot_title(slot: usize) -> &'static str {
    match slot {
        SLOT_INPUT => "Entrée",
        SLOT_MAIN => "Sortie principale",
        SLOT_PADS => "Sortie soundboard",
        _ => "Sortie monitoring",
    }
}

pub struct Engine {
    pub shared: Arc<Shared>,
    pub status: EngineStatus,
    plan: Plan,
    /// Flux ouverts (`SLOT_*`).
    streams: [Option<cpal::Stream>; SLOTS],
    /// Nom et canaux de chaque flux ouvert.
    opened: [Option<(String, u16)>; SLOTS],
    in_format: Option<SampleFormat>,
    /// Dernier échec d'ouverture de chaque flux : (message, périphérique introuvable).
    failed: [Option<(String, bool)>; SLOTS],
    in_tx: Option<Sender<InCmd>>,
    /// Commandes de chaque flux de sortie (celles du main portent le transport).
    out_tx: [Option<Sender<OutCmd>>; SLOTS],
    /// Commandes du soundboard (clonable pour le thread MIDI), tant qu'un flux les joue.
    pub pad_tx: Option<Sender<PadCmd>>,
    /// Canal du soundboard, le même pendant toute la vie du moteur (le thread MIDI garde son
    /// émetteur) : relu par le flux qui joue les pads, rouvert ou non.
    pad_chan: (Sender<PadCmd>, Receiver<PadCmd>),
    /// Le main joue les pads (pas de sortie soundboard à part, ou elle est absente).
    main_has_pads: bool,
    /// Périphériques retrouvés sous un autre nom, à signaler une fois.
    renamed: Vec<String>,
    /// Objets rendus par les callbacks pour être libérés hors du thread audio.
    garbage_tx: Sender<OutGarbage>,
    garbage_rx: Receiver<OutGarbage>,
    sample_garbage_tx: Sender<Arc<SampleData>>,
    sample_garbage_rx: Receiver<Arc<SampleData>>,
    in_garbage_tx: Sender<InGarbage>,
    in_garbage_rx: Receiver<InGarbage>,
}

/// (Ré)ouverture de flux préparée par l'interface (`Engine::prepare`) et exécutée hors de
/// son thread (`run`, pas de gel de l'écran), puis installée (`Engine::install`). Les flux
/// qui marchent ne sont jamais touchés.
pub struct ReopenJob {
    plan: Plan,
    slots: u8,
    /// Périphériques qui doivent être présents pour toucher aux flux de `gated`.
    require: u8,
    gated: u8,
    old: Vec<(usize, cpal::Stream)>,
    /// Périphériques déjà cherchés (ouverture complète : pas de seconde énumération).
    found: [Option<Found>; SLOTS],
    searched: u8,
    shared: Arc<Shared>,
    garbage_tx: Sender<OutGarbage>,
    in_garbage_tx: Sender<InGarbage>,
    sample_garbage_tx: Sender<Arc<SampleData>>,
    pad_rx: Receiver<PadCmd>,
    /// La sortie soundboard reste ouverte hors de cet essai : le main ne joue pas les pads.
    pads_elsewhere: bool,
    /// Nouveau tampon de monitoring (l'entrée ou la sortie qui le joue est rouverte).
    mon: Option<(Producer<f32>, Consumer<f32>)>,
}

/// Flux ouvert par un `ReopenJob`.
struct SlotOpen {
    stream: cpal::Stream,
    tx: SlotTx,
    name: String,
    id: Option<String>,
    channels: u16,
    fmt: Option<SampleFormat>,
    renamed: bool,
}

enum SlotTx {
    In(Sender<InCmd>),
    Out(Sender<OutCmd>),
}

type SlotResult = Result<SlotOpen, (String, bool)>;

/// Résultat d'un `ReopenJob`, à installer par `Engine::install`.
pub struct Reopened {
    slots: u8,
    results: [Option<SlotResult>; SLOTS],
    /// Flux rendus tels quels (périphérique requis absent).
    returned: Vec<(usize, cpal::Stream)>,
    /// Moitié du nouveau tampon de monitoring pour le flux resté ouvert.
    mon_to_input: Option<Producer<f32>>,
    mon_to_carrier: Option<Consumer<f32>>,
    /// Le nouveau main joue-t-il les pads ?
    main_pads: Option<bool>,
}

impl Reopened {
    /// Flux réellement rouverts (ou tentés) par cet essai.
    pub fn slots(&self) -> u8 {
        self.slots
    }
}

impl ReopenJob {
    /// Ouvre les flux demandés. À appeler hors du thread de l'interface (sauf ASIO et
    /// CoreAudio, qui restent sur le thread principal).
    pub fn run(self) -> Reopened {
        let ReopenJob {
            plan,
            mut slots,
            require,
            gated,
            old,
            mut found,
            searched,
            shared,
            garbage_tx,
            in_garbage_tx,
            sample_garbage_tx,
            pad_rx,
            pads_elsewhere,
            mon,
        } = self;
        let host = find_host(plan.host.as_deref());
        for (slot, f) in found.iter_mut().enumerate() {
            let bit = slot_bit(slot);
            if (slots | require) & bit != 0 && searched & bit == 0 && f.is_none() {
                *f = find_device(&host, &plan.want[slot], slot == SLOT_INPUT);
            }
        }
        // Sortie soundboard toujours absente : le main (qui joue les pads en attendant)
        // n'est pas touché, le reste de l'essai continue.
        let (old, returned): (Vec<_>, Vec<_>) =
            if (0..SLOTS).any(|s| require & slot_bit(s) != 0 && found[s].is_none()) {
                slots &= !gated;
                old.into_iter()
                    .partition(|(s, _)| gated & slot_bit(*s) == 0)
            } else {
                (old, Vec::new())
            };
        // Anciens flux fermés d'abord (ASIO : un seul pilote à la fois), puis leurs pertes
        // effacées : toute perte signalée ensuite vient des nouveaux flux.
        drop(old);
        shared.device_lost.fetch_and(!slots, Ordering::AcqRel);

        let has = |slot: usize| slots & slot_bit(slot) != 0;
        let absent = |slot: usize| {
            (
                format!(
                    "{} « {} » introuvable",
                    slot_title(slot),
                    plan.want[slot].label()
                ),
                true,
            )
        };
        let failed = |slot: usize, name: &str, e: anyhow::Error| {
            (format!("{} « {name} » : {e:#}", slot_title(slot)), false)
        };
        // Les pads repartent de zéro sur le flux qui les joue : appuis faits pendant la
        // coupure oubliés (ils partiraient tous d'un coup), positions remises à zéro.
        let reset_pads = || {
            while pad_rx.try_recv().is_ok() {}
            for p in &shared.pad_pos {
                p.store(0, Ordering::Relaxed);
            }
        };
        let idle = || bounded::<PadCmd>(1).1;
        let (mut mon_prod, mut mon_cons) = match mon {
            Some((p, c)) => (Some(p), Some(c)),
            None => (None, None),
        };
        let mut results: [Option<SlotResult>; SLOTS] = Default::default();
        let ctx = OutCtx {
            plan: &plan,
            shared: &shared,
            garbage: &garbage_tx,
            sample_garbage: &sample_garbage_tx,
        };
        if has(SLOT_INPUT) {
            results[SLOT_INPUT] = Some(match &found[SLOT_INPUT] {
                Some(f) => open_input(f, &plan, &shared, &in_garbage_tx, mon_prod.take())
                    .map_err(|e| failed(SLOT_INPUT, &f.name, e)),
                None => Err(absent(SLOT_INPUT)),
            });
        }
        let mut pads_ok = false;
        if has(SLOT_PADS) {
            results[SLOT_PADS] = Some(match &found[SLOT_PADS] {
                Some(f) => {
                    reset_pads();
                    let m = if plan.mon_on == SLOT_PADS {
                        mon_cons.take()
                    } else {
                        None
                    };
                    ctx.open(SLOT_PADS, f, pad_rx.clone(), m)
                        .map_err(|e| failed(SLOT_PADS, &f.name, e))
                }
                None => Err(absent(SLOT_PADS)),
            });
            pads_ok = matches!(results[SLOT_PADS], Some(Ok(_)));
        }
        // Sortie soundboard absente : les pads jouent sur le main en attendant (comme avant
        // la coupure), sauf si leur propre flux reste ouvert.
        let pads_on_main = !plan.want[SLOT_PADS].wanted() || !(pads_ok || pads_elsewhere);
        let mut main_pads = None;
        if has(SLOT_MAIN) {
            main_pads = Some(pads_on_main);
            results[SLOT_MAIN] = Some(match &found[SLOT_MAIN] {
                Some(f) => {
                    let pads = if pads_on_main {
                        reset_pads();
                        pad_rx.clone()
                    } else {
                        idle()
                    };
                    let m = if plan.mon_on == SLOT_MAIN {
                        mon_cons.take()
                    } else {
                        None
                    };
                    ctx.open(SLOT_MAIN, f, pads, m)
                        .map_err(|e| failed(SLOT_MAIN, &f.name, e))
                }
                None => Err(absent(SLOT_MAIN)),
            });
        }
        if has(SLOT_MON) {
            results[SLOT_MON] = Some(match &found[SLOT_MON] {
                Some(f) => {
                    let m = if plan.mon_on == SLOT_MON {
                        mon_cons.take()
                    } else {
                        None
                    };
                    ctx.open(SLOT_MON, f, idle(), m)
                        .map_err(|e| failed(SLOT_MON, &f.name, e))
                }
                None => Err(absent(SLOT_MON)),
            });
        }
        Reopened {
            slots,
            results,
            returned,
            mon_to_input: mon_prod,
            mon_to_carrier: mon_cons,
            main_pads,
        }
    }
}

/// Ouvre et démarre le flux d'entrée.
fn open_input(
    f: &Found,
    plan: &Plan,
    shared: &Arc<Shared>,
    garbage: &Sender<InGarbage>,
    monitor: Option<Producer<f32>>,
) -> Result<SlotOpen> {
    let sample_rate = plan.sample_rate;
    let (fmt, channels) = pick_config(&f.dev, sample_rate, true)?;
    let cfg = StreamConfig {
        channels,
        sample_rate,
        buffer_size: plan.buffer_size(),
    };
    let (tx, rx) = bounded(8);
    let st = InState {
        channels: channels as usize,
        rx,
        targets: Vec::with_capacity(MAX_INPUTS),
        garbage: garbage.clone(),
        shared: shared.clone(),
        monitor,
        sample_rate,
        last_capture: None,
        mon_env: 0.0,
        mon_release: mon_release(sample_rate),
        faulted: false,
    };
    let dev = &f.dev;
    let stream = match fmt {
        SampleFormat::F32 => build_input::<f32>(dev, cfg, st),
        SampleFormat::I32 => build_input::<i32>(dev, cfg, st),
        SampleFormat::I24 => build_input::<cpal::I24>(dev, cfg, st),
        SampleFormat::I16 => build_input::<i16>(dev, cfg, st),
        f => bail!("Format d'entrée non géré : {f:?}"),
    }
    .context("ouverture du flux d'entrée")?;
    stream.play().context("démarrage du flux d'entrée")?;
    Ok(SlotOpen {
        stream,
        tx: SlotTx::In(tx),
        name: f.name.clone(),
        id: f.id.clone(),
        channels,
        fmt: Some(fmt),
        renamed: f.renamed,
    })
}

/// Ce qu'il faut pour ouvrir un flux de sortie.
struct OutCtx<'a> {
    plan: &'a Plan,
    shared: &'a Arc<Shared>,
    garbage: &'a Sender<OutGarbage>,
    sample_garbage: &'a Sender<Arc<SampleData>>,
}

impl OutCtx<'_> {
    /// Ouvre et démarre la sortie `slot`. `pads` : commandes du soundboard (canal vide si ce
    /// flux ne le joue pas), `monitor` : tampon du monitoring s'il joue ici.
    fn open(
        &self,
        slot: usize,
        f: &Found,
        pads: Receiver<PadCmd>,
        monitor: Option<Consumer<f32>>,
    ) -> Result<SlotOpen> {
        let plan = self.plan;
        let sample_rate = plan.sample_rate;
        let (tx, rx) = bounded(8);
        let left = if slot == SLOT_MAIN { plan.out_left } else { 0 } as usize;
        let (pad_left, mon_left) = (plan.pad_left as usize, plan.mon_left as usize);
        let (garbage, sgt, sh) = (
            self.garbage.clone(),
            self.sample_garbage.clone(),
            self.shared.clone(),
        );
        let (stream, channels) =
            start_output(&f.dev, sample_rate, plan.buffer_size(), move |channels| {
                let last = channels.saturating_sub(1) as usize;
                let st = OutState::new(
                    channels as usize,
                    left.min(last),
                    rx,
                    garbage,
                    sh,
                    Sampler::new(pads, sgt, pad_left.min(last), sample_rate),
                )
                .with_monitor(monitor, mon_left)
                .for_slot(slot);
                if slot == SLOT_MAIN {
                    st.main_output(sample_rate)
                } else {
                    st
                }
            })?;
        Ok(SlotOpen {
            stream,
            tx: SlotTx::Out(tx),
            name: f.name.clone(),
            id: f.id.clone(),
            channels,
            fmt: None,
            renamed: f.renamed,
        })
    }
}

/// Nom d'un périphérique, sans jamais paniquer (un périphérique débranché pendant
/// l'énumération ne donne plus sa description).
fn dev_name(d: &cpal::Device) -> Option<String> {
    use std::fmt::Write;
    let mut s = String::new();
    write!(s, "{d}").ok()?;
    Some(s)
}

/// Nom comparable d'un périphérique : sans casse, et sans les préfixes « 2- », « 3- »…
/// que Windows ajoute quand une même carte revient sur un autre port USB
/// (« Haut-parleurs (2- Focusrite USB Audio) »).
fn normalize_device_name(s: &str) -> String {
    let c: Vec<char> = s.to_lowercase().chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < c.len() {
        let word_start = i == 0 || matches!(c[i - 1], '(' | ' ' | '[');
        if word_start && c[i].is_ascii_digit() {
            let mut j = i;
            while j < c.len() && j - i < 3 && c[j].is_ascii_digit() {
                j += 1;
            }
            if j + 1 < c.len() && c[j] == '-' && c[j + 1] == ' ' {
                i = j + 2;
                continue;
            }
        }
        out.push(c[i]);
        i += 1;
    }
    out.trim().to_string()
}

/// Choix parmi les périphériques présents `(identifiant, nom)` : identité stable d'abord,
/// puis nom exact, puis nom sans préfixe de port (seulement s'il n'y a qu'un candidat :
/// deux cartes identiques ne sont jamais confondues). Rend l'indice et « renommé ».
fn match_device(
    cands: &[(Option<String>, String)],
    name: Option<&str>,
    id: Option<&str>,
) -> Option<(usize, bool)> {
    if let Some(id) = id
        && let Some(i) = cands.iter().position(|(c, _)| c.as_deref() == Some(id))
    {
        return Some((i, name.is_some_and(|n| n != cands[i].1)));
    }
    let name = name?;
    if let Some(i) = cands.iter().position(|(_, n)| n == name) {
        return Some((i, false));
    }
    let norm = normalize_device_name(name);
    let mut it = cands
        .iter()
        .enumerate()
        .filter(|(_, (_, n))| normalize_device_name(n) == norm);
    match (it.next(), it.next()) {
        (Some((i, _)), None) => Some((i, true)),
        _ => None,
    }
}

/// Retrouve un périphérique (voir `match_device`). Énumération arrêtée dès le nom exact
/// quand aucune identité n'est connue : en ASIO, chaque périphérique énuméré charge son
/// pilote, on ne les charge pas tous pour rien.
fn find_device(host: &cpal::Host, want: &Want, input: bool) -> Option<Found> {
    let list = || {
        if input {
            host.input_devices()
        } else {
            host.output_devices()
        }
    };
    let asio = host.id().name() == "ASIO";
    let ident = |d: &cpal::Device| d.id().ok().map(|i| i.to_string());
    if !want.wanted() {
        // Défaut du système : figé sur le périphérique précis qu'il désigne maintenant
        // (WASAPI : sinon cpal suit le défaut et invalide le flux à chaque changement).
        let d = if input {
            host.default_input_device()
        } else {
            host.default_output_device()
        }?;
        if !asio
            && let Ok(id) = d.id()
            && let Some(x) = list()
                .ok()
                .and_then(|mut it| it.find(|x| x.id().ok().as_ref() == Some(&id)))
        {
            let name = dev_name(&x)?;
            return Some(Found {
                dev: x,
                name,
                id: Some(id.to_string()),
                renamed: false,
            });
        }
        let name = dev_name(&d)?;
        return Some(Found {
            id: ident(&d),
            dev: d,
            name,
            renamed: false,
        });
    }
    let id = want.id.as_deref().filter(|_| !asio);
    let mut devs = Vec::new();
    let mut cands = Vec::new();
    for d in list().ok()? {
        let Some(n) = dev_name(&d) else {
            continue;
        };
        if id.is_none() && want.name.as_deref() == Some(n.as_str()) {
            return Some(Found {
                id: ident(&d),
                dev: d,
                name: n,
                renamed: false,
            });
        }
        cands.push((if asio { None } else { ident(&d) }, n));
        devs.push(d);
    }
    let (i, renamed) = match_device(&cands, want.name.as_deref(), id)?;
    let (cid, name) = cands.swap_remove(i);
    Some(Found {
        dev: devs.swap_remove(i),
        name,
        id: cid,
        renamed,
    })
}

impl Engine {
    fn with_plan(plan: Plan) -> Self {
        let (garbage_tx, garbage_rx) = bounded(16);
        let (in_garbage_tx, in_garbage_rx) = bounded(16);
        let (sample_garbage_tx, sample_garbage_rx) = bounded(256);
        let status = EngineStatus {
            host: plan.host_name.clone(),
            sample_rate: plan.sample_rate,
            buffer: plan.buffer,
            ..Default::default()
        };
        Self {
            shared: Arc::new(Shared::new()),
            status,
            plan,
            streams: Default::default(),
            opened: Default::default(),
            in_format: None,
            failed: Default::default(),
            in_tx: None,
            out_tx: Default::default(),
            pad_tx: None,
            pad_chan: bounded(256),
            main_has_pads: false,
            renamed: Vec::new(),
            garbage_tx,
            garbage_rx,
            sample_garbage_tx,
            sample_garbage_rx,
            in_garbage_tx,
            in_garbage_rx,
        }
    }

    /// Moteur sans carte son (en cas d'échec d'ouverture).
    pub fn offline() -> Self {
        Self::with_plan(Plan {
            host: None,
            host_name: String::new(),
            sample_rate: 48000,
            buffer: None,
            want: Default::default(),
            mon_on: SLOT_MAIN,
            defaulted: 0,
            out_left: 0,
            pad_left: 0,
            mon_left: 0,
        })
    }

    /// Ouvre la carte. `pad` = sortie du soundboard. Un flux qui ne s'ouvre pas (entrée
    /// refusée, périphérique absent) n'empêche jamais les autres de marcher : il est noté
    /// (`missing`, `problems`) et l'interface réessaie de l'ouvrir.
    pub fn open(prefs: &AudioPrefs, pad: &PadOut) -> Result<Self> {
        let host = find_host(prefs.host.as_deref());
        let host_name = host.id().name().to_string();
        let asio = host_name == "ASIO";
        let fin = find_device(&host, &Want::named(prefs.input.clone()), true);
        // ASIO : entrée et sorties sur le même pilote → le même périphérique, sans nouvelle
        // énumération (chaque énumération ASIO charge les pilotes installés un à un).
        let reuse = |name: &Option<String>| asio && name.is_some() && *name == prefs.input;
        let fout = if reuse(&prefs.output) {
            fin.clone()
        } else {
            find_device(&host, &Want::named(prefs.output.clone()), false)
        };

        // Fréquence : celle demandée, sinon celle par défaut de l'entrée.
        let sample_rate = prefs
            .sample_rate
            .or_else(|| {
                fin.as_ref()
                    .and_then(|f| f.dev.default_input_config().ok())
                    .map(|c| c.sample_rate())
            })
            .or_else(|| {
                fout.as_ref()
                    .and_then(|f| f.dev.default_output_config().ok())
                    .map(|c| c.sample_rate())
            })
            .unwrap_or(48000);

        let ident = |f: &Option<Found>, name: &Option<String>| match f {
            Some(f) => Want {
                name: Some(f.name.clone()),
                id: f.id.clone(),
            },
            None => Want::named(name.clone()),
        };
        // Soundboard sur un autre périphérique que l'écoute : second flux de sortie.
        // Monitoring : sur le flux d'écoute, sur celui du soundboard, ou sur un flux à part.
        let main_name = fout
            .as_ref()
            .map(|f| f.name.clone())
            .or_else(|| prefs.output.clone());
        let pads_sep = pad.device.clone().filter(|n| Some(n) != main_name.as_ref());
        let mon_name = prefs.monitor_output.clone().or_else(|| main_name.clone());
        let mon_on = if mon_name == main_name {
            SLOT_MAIN
        } else if pads_sep.is_some() && mon_name == pads_sep {
            SLOT_PADS
        } else {
            SLOT_MON
        };
        let mut want: [Want; SLOTS] = Default::default();
        want[SLOT_INPUT] = ident(&fin, &prefs.input);
        want[SLOT_MAIN] = ident(&fout, &prefs.output);
        want[SLOT_PADS] = Want::named(pads_sep);
        if mon_on == SLOT_MON {
            want[SLOT_MON] = Want::named(mon_name);
        }
        let mut found: [Option<Found>; SLOTS] = Default::default();
        let mut searched = LOST_INPUT | LOST_OUTPUT;
        for s in [SLOT_PADS, SLOT_MON] {
            if reuse(&want[s].name) {
                found[s] = fin.clone();
                searched |= slot_bit(s);
            }
        }
        let mut defaulted = 0;
        if prefs.input.is_none() && fin.is_some() {
            defaulted |= LOST_INPUT;
        }
        if prefs.output.is_none() && fout.is_some() {
            defaulted |= LOST_OUTPUT;
        }
        found[SLOT_INPUT] = fin;
        found[SLOT_MAIN] = fout;
        let mut e = Self::with_plan(Plan {
            host: prefs.host.clone(),
            host_name,
            sample_rate,
            buffer: prefs.buffer,
            want,
            mon_on,
            defaulted,
            out_left: prefs.out_left,
            pad_left: pad.left,
            mon_left: prefs.monitor_pair(),
        });
        let mut job = e.prepare(LOST_ALL);
        job.found = found;
        job.searched = searched;
        let done = job.run();
        e.install(done);
        Ok(e)
    }

    /// Flux ouverts (bits `LOST_*`).
    pub fn open_mask(&self) -> u8 {
        (0..SLOTS)
            .filter(|&s| self.streams[s].is_some())
            .fold(0, |m, s| m | slot_bit(s))
    }

    /// Flux attendus mais pas ouverts (périphérique absent, ouverture refusée, ou en cours
    /// de réouverture).
    pub fn missing(&self) -> u8 {
        self.plan.wanted() & !self.open_mask()
    }

    /// Flux qui joue le soundboard (bit `LOST_*`, 0 = aucun).
    pub fn pads_slot_bit(&self) -> u8 {
        if self.streams[SLOT_PADS].is_some() {
            LOST_PADS
        } else if self.main_has_pads && self.streams[SLOT_MAIN].is_some() {
            LOST_OUTPUT
        } else {
            0
        }
    }

    /// Les pads jouent sur le main faute de leur propre sortie.
    pub fn pads_folded(&self) -> bool {
        self.main_has_pads && self.plan.want[SLOT_PADS].wanted()
    }

    /// Ce qui ne marche pas, en clair : flux perdus (`lost`) et flux absents.
    pub fn problems(&self, lost: u8) -> Vec<String> {
        let missing = self.missing();
        let mut out = Vec::new();
        for slot in 0..SLOTS {
            let bit = slot_bit(slot);
            if missing & bit != 0
                && let Some((t, absent)) = &self.failed[slot]
            {
                // Réglé sur « Par défaut » : la carte d'origine est attendue, jamais remplacée
                // en silence par le nouveau défaut de Windows (casque Bluetooth, webcam).
                if *absent && self.plan.defaulted & bit != 0 {
                    out.push(format!(
                        "{t} (réglage « Par défaut » : la carte d'origine est attendue ; pour suivre le nouveau périphérique par défaut de Windows, cliquez sur « Actualiser » dans les réglages audio)"
                    ));
                } else {
                    out.push(t.clone());
                }
            } else if (lost | missing) & bit != 0 {
                out.push(format!(
                    "{} « {} » perdue",
                    slot_title(slot),
                    self.plan.want[slot].label()
                ));
            }
        }
        if self.pads_folded() && missing & LOST_PADS != 0 {
            out.push("pads joués sur l'écoute en attendant".into());
        }
        out
    }

    /// Échecs d'ouverture d'un périphérique présent (réglage refusé…), à corriger dans les
    /// réglages : `None` si tout est ouvert ou seulement absent.
    pub fn open_errors(&self) -> Option<String> {
        let v: Vec<&str> = self
            .failed
            .iter()
            .flatten()
            .filter(|(_, absent)| !absent)
            .map(|(t, _)| t.as_str())
            .collect();
        (!v.is_empty()).then(|| v.join(" · "))
    }

    /// Flux dont le périphérique est présent mais a refusé de s'ouvrir (bits `LOST_*`).
    pub fn refused(&self) -> u8 {
        (0..SLOTS)
            .filter(|&s| matches!(self.failed[s], Some((_, false))))
            .fold(0, |m, s| m | slot_bit(s))
    }

    /// Périphériques retrouvés sous un autre nom depuis le dernier appel.
    pub fn take_renamed(&mut self) -> Vec<String> {
        std::mem::take(&mut self.renamed)
    }

    /// Flux réellement touchés par la réouverture de `slots`, et ceux dont le périphérique
    /// doit être présent avant de toucher aux flux de `gated`. Pads repliés sur le main et
    /// sortie soundboard de retour : le main doit être rouvert lui aussi (un seul lecteur
    /// de pads à la fois), seulement si cette sortie est bien revenue.
    pub fn expand(&self, slots: u8) -> (u8, u8, u8) {
        let mut slots = slots & self.plan.wanted();
        let (mut require, mut gated) = (0, 0);
        if slots & LOST_PADS != 0
            && self.main_has_pads
            && self.streams[SLOT_MAIN].is_some()
            && slots & LOST_OUTPUT == 0
        {
            slots |= LOST_OUTPUT;
            require = LOST_PADS;
            gated = LOST_PADS | LOST_OUTPUT;
        }
        (slots, require, gated)
    }

    /// Prépare la réouverture de `slots` (voir `expand`) : les flux concernés sont retirés
    /// du moteur (fermés par l'essai), les autres continuent sans coupure.
    pub fn prepare(&mut self, slots: u8) -> ReopenJob {
        let (slots, require, gated) = self.expand(slots);
        let old = (0..SLOTS)
            .filter(|&s| slots & slot_bit(s) != 0)
            .filter_map(|s| self.streams[s].take().map(|st| (s, st)))
            .collect();
        // Entrée ou sortie du monitoring rouverte : nouveau tampon entre les deux.
        let mon = (slots & (LOST_INPUT | slot_bit(self.plan.mon_on)) != 0)
            .then(|| rtrb::RingBuffer::<f32>::new(MON_RING * 2));
        ReopenJob {
            plan: self.plan.clone(),
            slots,
            require,
            gated,
            old,
            found: Default::default(),
            searched: 0,
            shared: self.shared.clone(),
            garbage_tx: self.garbage_tx.clone(),
            in_garbage_tx: self.in_garbage_tx.clone(),
            sample_garbage_tx: self.sample_garbage_tx.clone(),
            pad_rx: self.pad_chan.1.clone(),
            pads_elsewhere: self.streams[SLOT_PADS].is_some(),
            mon,
        }
    }

    /// Installe le résultat d'un essai : flux rouverts à leur place, monitoring rebranché,
    /// échecs notés. À appeler sur le thread de l'interface.
    pub fn install(&mut self, r: Reopened) {
        let Reopened {
            slots: _,
            mut results,
            returned,
            mon_to_input,
            mon_to_carrier,
            main_pads,
        } = r;
        for (s, st) in returned {
            self.streams[s] = Some(st);
        }
        for (slot, res) in results.iter_mut().enumerate() {
            let Some(res) = res.take() else {
                continue;
            };
            match res {
                Ok(o) => {
                    let asked = self.plan.want[slot].name.clone();
                    if o.renamed {
                        self.renamed.push(format!(
                            "{} « {} » retrouvée sous le nom « {} »",
                            slot_title(slot),
                            asked.as_deref().unwrap_or("?"),
                            o.name
                        ));
                    }
                    self.plan.want[slot] = Want {
                        name: Some(o.name.clone()),
                        id: o.id,
                    };
                    self.opened[slot] = Some((o.name, o.channels));
                    match o.tx {
                        SlotTx::In(tx) => {
                            self.in_tx = Some(tx);
                            self.in_format = o.fmt;
                        }
                        SlotTx::Out(tx) => self.out_tx[slot] = Some(tx),
                    }
                    self.failed[slot] = None;
                    self.streams[slot] = Some(o.stream);
                }
                Err(f) => {
                    self.streams[slot] = None;
                    self.opened[slot] = None;
                    if slot == SLOT_INPUT {
                        self.in_tx = None;
                        self.in_format = None;
                    } else {
                        self.out_tx[slot] = None;
                    }
                    self.failed[slot] = Some(f);
                }
            }
        }
        if let Some(m) = main_pads {
            self.main_has_pads = m && self.streams[SLOT_MAIN].is_some();
        }
        // L'autre moitié du nouveau tampon de monitoring part au flux resté ouvert (s'il ne
        // la reçoit pas, elle est libérée ici, hors du thread audio).
        if let (Some(p), Some(tx)) = (mon_to_input, &self.in_tx) {
            let _ = tx.try_send(InCmd::Monitor(p));
        }
        let carrier = self.plan.mon_on;
        if let (Some(c), Some(tx), Some((_, ch))) =
            (mon_to_carrier, &self.out_tx[carrier], &self.opened[carrier])
        {
            let left = (self.plan.mon_left as usize).min(ch.saturating_sub(1) as usize);
            let _ = tx.try_send(OutCmd::Monitor(MonitorOut::new(c, left)));
        }
        self.pad_tx = (self.pads_slot_bit() != 0).then(|| self.pad_chan.0.clone());
        if self.shared.device_lost.load(Ordering::Acquire) == 0 {
            self.shared.stream_error.store(0, Ordering::Release);
        }
        self.refresh_status();
    }

    fn refresh_status(&mut self) {
        let name = |s: usize| self.opened[s].as_ref().map(|o| o.0.clone());
        let ch = |s: usize| self.opened[s].as_ref().map_or(0, |o| o.1);
        let st = &mut self.status;
        st.host = self.plan.host_name.clone();
        st.sample_rate = self.plan.sample_rate;
        st.buffer = self.plan.buffer;
        st.input = name(SLOT_INPUT);
        st.in_channels = ch(SLOT_INPUT);
        st.in_format = self.in_format;
        st.output = name(SLOT_MAIN);
        st.out_channels = ch(SLOT_MAIN);
        st.pad_output = name(SLOT_PADS);
        st.pad_out_channels = if self.opened[SLOT_PADS].is_some() {
            ch(SLOT_PADS)
        } else {
            ch(SLOT_MAIN)
        };
        st.monitor_output = name(SLOT_MON);
        st.monitor_ok = self.opened[self.plan.mon_on].is_some();
        let mut errs: Vec<String> = self.failed.iter().flatten().map(|f| f.0.clone()).collect();
        if self.pads_folded() {
            errs.push("pads joués sur l'écoute".into());
        }
        *self.shared.last_error.lock() = (!errs.is_empty()).then(|| errs.join(" · "));
    }

    pub fn has_input(&self) -> bool {
        self.in_tx.is_some()
    }

    pub fn has_output(&self) -> bool {
        self.out_tx[SLOT_MAIN].is_some()
    }

    pub fn start_record(&self, targets: Vec<RecTarget>) -> Result<()> {
        let tx = self
            .in_tx
            .as_ref()
            .ok_or_else(|| anyhow!("Aucune entrée audio"))?;
        self.shared.rec_frames.store(0, Ordering::Relaxed);
        self.shared.rec_gap_frames.store(0, Ordering::Relaxed);
        // Compteurs propres à cette prise.
        for c in [
            &self.shared.xruns,
            &self.shared.underruns,
            &self.shared.overruns,
            &self.shared.copy_overruns,
        ] {
            c.store(0, Ordering::Relaxed);
        }
        self.shared.counting.store(true, Ordering::Relaxed);
        tx.try_send(InCmd::Record(targets))
            .map_err(|_| anyhow!("Moteur audio occupé"))
    }

    /// Demande l'arrêt de l'enregistrement et attend que le callback rende les cibles
    /// (jusqu'à `timeout`). Leurs producteurs sont libérés ici, hors du thread audio :
    /// le thread disque voit alors la fin de prise et écrit tout ce qui reste.
    pub fn stop_record(&self, timeout: std::time::Duration) -> bool {
        let Some(tx) = &self.in_tx else {
            return false;
        };
        let deadline = std::time::Instant::now() + timeout;
        // Envoi fiable même si la file était momentanément pleine.
        while tx.try_send(InCmd::Stop).is_err() {
            if std::time::Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        // Attente par sondage (pas de `recv_timeout`) : sans récepteur endormi, l'envoi
        // côté callback audio ne prend aucun verrou et ne réveille aucun thread.
        loop {
            match self.in_garbage_rx.try_recv() {
                // Liste vide = celle remplacée au démarrage ; on attend celle de la prise.
                Ok(InGarbage::Targets(v)) if v.is_empty() => continue,
                Ok(InGarbage::Targets(_)) => return true,
                Ok(InGarbage::Monitor(_)) => continue,
                Err(crossbeam_channel::TryRecvError::Disconnected) => return false,
                Err(crossbeam_channel::TryRecvError::Empty) => {
                    if std::time::Instant::now() > deadline {
                        return false;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
        }
    }

    /// `false` si la sortie ne répond pas (commande non transmise).
    pub fn start_playback(&self, cons: Consumer<f32>) -> bool {
        // Remis à zéro ici et dans le callback : pas de saut de la tête au démarrage.
        self.shared.out_cb_ns.store(0, Ordering::Release);
        self.shared.out_frames.store(0, Ordering::Relaxed);
        self.shared.counting.store(true, Ordering::Relaxed);
        self.send_out(OutCmd::Play(cons))
    }

    /// Envoi fiable d'une commande de transport : si la file est momentanément pleine
    /// (callback en retard), on réessaie un court instant au lieu de perdre un Stop
    /// (la lecture continuerait) ou un Play (l'interface croirait lire).
    fn send_out(&self, mut cmd: OutCmd) -> bool {
        let Some(tx) = &self.out_tx[SLOT_MAIN] else {
            return false;
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
        loop {
            match tx.try_send(cmd) {
                Ok(()) => return true,
                Err(crossbeam_channel::TrySendError::Full(c)) => {
                    if std::time::Instant::now() > deadline {
                        return false;
                    }
                    cmd = c;
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(crossbeam_channel::TrySendError::Disconnected(_)) => return false,
            }
        }
    }

    /// Bascule sans coupure vers un nouveau rendu de la lecture en cours, qui commence à la
    /// trame `at` de cette lecture (mute / solo). `false` si la commande n'a pas pu partir.
    pub fn switch_playback(&self, cons: Consumer<f32>, at: u64) -> bool {
        self.out_tx[SLOT_MAIN]
            .as_ref()
            .is_some_and(|tx| tx.try_send(OutCmd::Switch(cons, at)).is_ok())
    }

    pub fn stop_playback(&self) {
        self.send_out(OutCmd::Stop);
    }

    /// À appeler régulièrement depuis l'interface.
    pub fn collect_garbage(&self) {
        while self.garbage_rx.try_recv().is_ok() {}
        while self.sample_garbage_rx.try_recv().is_ok() {}
        while self.in_garbage_rx.try_recv().is_ok() {}
    }

    /// Commande de pad qui ne doit jamais se perdre (échange de places : sinon les sons en
    /// cours resteraient sur les anciennes cases) : réessayée un court instant.
    pub fn pad_reliable(&self, mut cmd: PadCmd) {
        let Some(tx) = &self.pad_tx else {
            return;
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
        loop {
            match tx.try_send(cmd) {
                Ok(()) => return,
                Err(crossbeam_channel::TrySendError::Full(c))
                    if std::time::Instant::now() < deadline =>
                {
                    cmd = c;
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(_) => return,
            }
        }
    }

    pub fn pad(&self, cmd: PadCmd) {
        if let Some(tx) = &self.pad_tx {
            let _ = tx.try_send(cmd);
        }
    }

    /// Latence aller-retour du pilote (trames, 0 = inconnue) : voir `Shared`.
    pub fn round_trip_latency(&self) -> u32 {
        self.shared.round_trip_latency()
    }
}

/// Fréquence lisible (« 44,1 », « 48 »).
fn khz(sr: u32) -> String {
    if sr.is_multiple_of(1000) {
        format!("{}", sr / 1000)
    } else {
        format!("{:.1}", sr as f64 / 1000.0).replace('.', ",")
    }
}

/// Choisit le format d'échantillon et le nombre de canaux maximal pour une fréquence.
fn pick_config(dev: &cpal::Device, sr: u32, input: bool) -> Result<(SampleFormat, u16)> {
    let ranges: Vec<_> = if input {
        dev.supported_input_configs()?.collect()
    } else {
        dev.supported_output_configs()?.collect()
    };
    let rank = |f: SampleFormat| match f {
        SampleFormat::F32 => 0,
        SampleFormat::I32 => 1,
        SampleFormat::I24 => 2,
        SampleFormat::I16 => 3,
        _ => 99,
    };
    ranges
        .iter()
        .filter(|r| r.min_sample_rate() <= sr && sr <= r.max_sample_rate())
        .filter(|r| rank(r.sample_format()) < 99)
        .max_by_key(|r| (r.channels(), -rank(r.sample_format())))
        .map(|r| (r.sample_format(), r.channels()))
        .ok_or_else(|| {
            // Fréquence changée dans le pilote ou dans Windows (WASAPI partagé n'accepte que
            // celle-là) : on le dit clairement, sans changer de fréquence en silence.
            let cur = if input {
                dev.default_input_config()
            } else {
                dev.default_output_config()
            }
            .ok()
            .map(|c| c.sample_rate());
            match cur {
                Some(c) if c != sr => anyhow!(
                    "la carte tourne en {} kHz (réglage du pilote ou de Windows) mais TracksPlus est en {} kHz : remettez {} kHz",
                    khz(c),
                    khz(sr),
                    khz(sr)
                ),
                _ => anyhow!("ne supporte pas {} kHz", khz(sr)),
            }
        })
}

struct InState {
    channels: usize,
    rx: Receiver<InCmd>,
    targets: Vec<RecTarget>,
    /// Les listes de cibles remplacées sont rendues à l'interface pour y être libérées.
    garbage: Sender<InGarbage>,
    shared: Arc<Shared>,
    /// Vers la sortie de monitoring (mix stéréo entrelacé des canaux écoutés).
    monitor: Option<Producer<f32>>,
    sample_rate: u32,
    /// Bloc précédent : instant de capture de sa première trame et nombre de trames.
    last_capture: Option<(cpal::StreamInstant, usize)>,
    /// Limiteur du monitoring : enveloppe de crête et coefficient de retour par trame.
    mon_env: f32,
    mon_release: f32,
    /// Panique rattrapée : le flux ne fait plus rien (il va être rouvert).
    faulted: bool,
}

/// Coefficient de retour du limiteur de monitoring (~150 ms).
fn mon_release(sample_rate: u32) -> f32 {
    (-1.0 / (0.15 * sample_rate.max(1) as f32)).exp()
}

impl InState {
    /// Callback d'entrée complet. `ts` = (capture, callback) d'après le pilote. Une
    /// panique (cas non prévu) ne remonte jamais dans le pilote (en ASIO, elle fermerait
    /// l'application, prises comprises) : le flux se tait et est signalé perdu. Aucun coût
    /// ni allocation sur le chemin normal.
    fn callback<T>(&mut self, data: &[T], ts: Option<(cpal::StreamInstant, cpal::StreamInstant)>)
    where
        T: SizedSample,
        f32: FromSample<T>,
    {
        self.shared.heartbeat[SLOT_INPUT].fetch_add(1, Ordering::Relaxed);
        if self.faulted {
            return;
        }
        let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if let Some((capture, callback)) = ts {
                // Latence capture → callback (compensation des prises en re-recording).
                self.shared.in_latency.store(
                    to_frames(callback.duration_since(capture), self.sample_rate),
                    Ordering::Relaxed,
                );
                if let Some(frames) = data.len().checked_div(self.channels) {
                    self.check_capture_gap(capture, frames);
                }
            }
            self.process(data);
        }))
        .is_ok();
        if !ok {
            self.faulted = true;
            self.shared.fault(LOST_INPUT);
        }
    }

    /// Décrochage non signalé par le pilote : l'instant de capture d'un bloc doit suivre
    /// celui du précédent de sa durée exacte, quelle que soit la régularité de livraison
    /// (rafales comprises). Un saut de plus de deux blocs (et de 10 ms) au-delà veut dire
    /// que des échantillons ont été perdus en amont.
    ///
    /// Pendant une prise, un vrai trou (horloge Dante perdue quelques secondes, pilote figé)
    /// est comblé par autant de silence : la suite de la prise reste calée sur l'heure
    /// réelle, donc sur la timeline et les autres pistes.
    fn check_capture_gap(&mut self, capture: cpal::StreamInstant, frames: usize) {
        if let Some((prev, n)) = self.last_capture
            && let Some(dt) = capture.checked_duration_since(prev)
            && self.sample_rate > 0
        {
            let expected = n as f64 / self.sample_rate as f64;
            // Au moins deux blocs et 10 ms : l'horloge système de certains pilotes ASIO
            // est grossière (pas de fausse alerte), une vraie coupure dépasse largement.
            let tolerance = (2.0 * frames as f64 / self.sample_rate as f64).max(0.010);
            let late = dt.as_secs_f64() - expected;
            if late > tolerance && self.shared.counting.load(Ordering::Relaxed) {
                self.shared.xruns.fetch_add(1, Ordering::Relaxed);
            }
            if late >= GAP_FILL_MIN && !self.targets.is_empty() {
                self.fill_gap(late, frames);
            }
        }
        self.last_capture = Some((capture, frames));
    }

    /// Insère `secs` de silence dans toutes les pistes de la prise, la même durée partout
    /// (synchronisation entre pistes), dans la place libre des tampons seulement : le son
    /// qui revient n'est jamais sacrifié pour combler le trou.
    fn fill_gap(&mut self, secs: f64, frames: usize) {
        let want = (secs.min(GAP_FILL_MAX) * self.sample_rate as f64) as usize;
        let room = |p: &Producer<f32>, w: usize, pending: usize| {
            (p.slots() / w.max(1)).saturating_sub(pending)
        };
        let free = self
            .targets
            .iter()
            .flat_map(|t| {
                let main = room(&t.prod, t.width, t.pending);
                let copy = t.copy.as_ref().map(|(p, pd)| room(p, t.width, *pd));
                [Some(main), copy]
            })
            .flatten()
            .min()
            .unwrap_or(0);
        // Le bloc qui arrive doit encore tenir derrière le silence.
        let n = want.min(free.saturating_sub(2 * frames));
        if n == 0 {
            return;
        }
        for t in &mut self.targets {
            t.pending += n;
            if let Some((_, p)) = &mut t.copy {
                *p += n;
            }
        }
        let sh = &self.shared;
        sh.rec_frames.fetch_add(n as u64, Ordering::Release);
        sh.rec_gap_frames.fetch_add(n as u64, Ordering::Relaxed);
    }

    fn dispose(&self, v: Vec<RecTarget>) {
        if let Err(e) = self.garbage.try_send(InGarbage::Targets(v)) {
            std::mem::forget(e.into_inner());
        }
    }
}

impl InState {
    /// Corps du callback d'entrée (hors horodatage) : commandes, vumètres, monitoring,
    /// écriture vers les tampons disque. Séparé pour être testé sans carte son.
    fn process<T>(&mut self, data: &[T])
    where
        T: SizedSample,
        f32: FromSample<T>,
    {
        let st = self;
        while let Ok(cmd) = st.rx.try_recv() {
            // Aucune libération mémoire ici : l'ancienne liste part vers l'interface,
            // qui relâche les producteurs (le thread disque voit alors la fin de prise).
            match cmd {
                InCmd::Record(t) => {
                    st.shared.rec_play_offset.store(
                        st.shared.play_position_now(st.sample_rate),
                        Ordering::Relaxed,
                    );
                    let old = std::mem::replace(&mut st.targets, t);
                    st.dispose(old);
                }
                InCmd::Stop => {
                    let old = std::mem::take(&mut st.targets);
                    st.dispose(old);
                }
                InCmd::Monitor(p) => {
                    if let Some(old) = st.monitor.replace(p)
                        && let Err(e) = st.garbage.try_send(InGarbage::Monitor(old))
                    {
                        std::mem::forget(e.into_inner());
                    }
                }
            }
        }
        let ch = st.channels;
        if ch == 0 {
            return;
        }
        let frames = data.len() / ch;
        let sh = &*st.shared;
        sh.in_block.store(frames as u32, Ordering::Relaxed);

        // Crêtes par canal en un seul passage dans l'ordre de la mémoire (au lieu d'un
        // passage par canal en sautant de trame en trame : 64× moins de lectures).
        let mc = ch.min(MAX_INPUTS);
        let mut peaks = [0f32; MAX_INPUTS];
        for frame in data.chunks_exact(ch) {
            for (p, s) in peaks[..mc].iter_mut().zip(&frame[..mc]) {
                *p = p.max(s.to_sample::<f32>().abs());
            }
        }
        for (m, p) in sh.meters.iter().zip(&peaks[..mc]) {
            m.fetch_max(p.to_bits(), Ordering::Relaxed);
        }

        // Monitoring direct : mix stéréo des canaux écoutés vers la sortie.
        if sh.monitor_on.load(Ordering::Relaxed)
            && let Some(mp) = st.monitor.as_mut()
        {
            // Canaux écoutés, relevés une fois par bloc (tableau sur la pile).
            let mut act = [(0usize, 0u8); MAX_INPUTS];
            let mut na = 0;
            for c in 0..ch.min(MAX_INPUTS) {
                let m = sh.monitor[c].load(Ordering::Relaxed);
                if m != 0 {
                    act[na] = (c, m);
                    na += 1;
                }
            }
            let n = frames.min(mp.slots() / 2);
            if n > 0
                && let Ok(chunk) = mp.write_chunk_uninit(n * 2)
            {
                // Comme sur une console : une entrée mono au centre à -3 dB (loi de
                // panoramique à puissance constante), une paire stéréo à 0 dB de chaque côté.
                // Puis limiteur de sécurité à -1 dBFS sur ce mix d'écoute seulement (les
                // prises ne sont jamais touchées) : plusieurs entrées écoutées ensemble ne
                // saturent pas le casque ou la façade.
                const CENTER: f32 = std::f32::consts::FRAC_1_SQRT_2;
                const LIMIT: f32 = 0.891; // -1 dBFS
                let rel = st.mon_release;
                let mut env = st.mon_env;
                chunk.fill_from_iter((0..n).flat_map(|f| {
                    let (mut l, mut r) = (0f32, 0f32);
                    for &(c, m) in &act[..na] {
                        let s = data[f * ch + c].to_sample::<f32>();
                        match m {
                            2 => l += s,
                            3 => r += s,
                            _ => {
                                l += s * CENTER;
                                r += s * CENTER;
                            }
                        }
                    }
                    // Attaque instantanée (rien ne dépasse), retour en ~150 ms.
                    let peak = l.abs().max(r.abs());
                    env = if peak.is_finite() {
                        peak.max(env * rel)
                    } else {
                        env
                    };
                    let g = if env > LIMIT { LIMIT / env } else { 1.0 };
                    [l * g, r * g]
                }));
                st.mon_env = env;
            }
        }

        if !st.targets.is_empty() {
            for t in &mut st.targets {
                let cs = t.channels;
                let sample = |f: usize, k: usize| {
                    let c = cs[k];
                    if c < ch {
                        data[f * ch + c].to_sample::<f32>()
                    } else {
                        0.0
                    }
                };
                if feed(&mut t.prod, &mut t.pending, t.width, frames, &sample) {
                    sh.overruns.fetch_add(1, Ordering::Relaxed);
                }
                if let Some((p, pending)) = &mut t.copy
                    && feed(p, pending, t.width, frames, &sample)
                {
                    sh.copy_overruns.fetch_add(1, Ordering::Relaxed);
                }
            }
            sh.rec_frames.fetch_add(frames as u64, Ordering::Release);
        }
    }
}

fn build_input<T>(dev: &cpal::Device, cfg: StreamConfig, mut st: InState) -> Result<cpal::Stream>
where
    T: SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    let err_shared = st.shared.clone();
    let stream = dev.build_input_stream::<T, _, _>(
        cfg,
        move |data: &[T], info: &cpal::InputCallbackInfo| {
            let ts = info.timestamp();
            st.callback(data, Some((ts.capture, ts.callback)));
        },
        move |e| err_shared.on_error(e, LOST_INPUT),
        Some(OPEN_TIMEOUT),
    )?;
    Ok(stream)
}

/// Écrit un bloc d'une entrée dans son tampon disque, sans jamais décaler la piste :
/// si des trames ont dû être perdues (disque en retard), autant de silence est inséré
/// à leur place dès que le tampon a de nouveau de la place. Retourne `true` en cas de perte.
/// `sample(trame, k)` donne l'échantillon du k-ième canal de la cible. Tout se compte en
/// trames entières : une piste stéréo ne se désentrelace jamais.
#[cfg(test)]
fn feed_target(t: &mut RecTarget, frames: usize, sample: impl Fn(usize, usize) -> f32) -> bool {
    feed(&mut t.prod, &mut t.pending, t.width, frames, &sample)
}

/// Écrit un bloc dans un tampon disque (voir `feed_target`).
fn feed(
    prod: &mut Producer<f32>,
    pending: &mut usize,
    width: usize,
    frames: usize,
    sample: &impl Fn(usize, usize) -> f32,
) -> bool {
    let w = width.max(1);
    if *pending > 0 {
        let z = (*pending).min(prod.slots() / w);
        if let Ok(chunk) = prod.write_chunk_uninit(z * w) {
            chunk.fill_from_iter(std::iter::repeat_n(0.0f32, z * w));
        }
        *pending -= z;
    }
    let n = if *pending > 0 {
        0
    } else {
        frames.min(prod.slots() / w)
    };
    if n > 0
        && let Ok(chunk) = prod.write_chunk_uninit(n * w)
    {
        // Cas courants spécialisés (pas de division par échantillon).
        match w {
            1 => chunk.fill_from_iter((0..n).map(|f| sample(f, 0))),
            2 => chunk.fill_from_iter((0..n).flat_map(|f| [sample(f, 0), sample(f, 1)])),
            _ => chunk.fill_from_iter((0..n * w).map(|i| sample(i / w, i % w))),
        };
    }
    if n < frames {
        *pending += frames - n;
        return true;
    }
    false
}

const MAX_BLOCK: usize = 4096;
/// Capacité du tampon de monitoring (trames stéréo) : large, pour que la cible et le
/// seuil de rattrapage restent atteignables même avec de très gros buffers.
const MON_RING: usize = 65536;

struct OutState {
    channels: usize,
    left: usize,
    rx: Receiver<OutCmd>,
    cons: Option<Consumer<f32>>,
    /// Rendu qui prendra le relais (mute / solo) et trame de la bascule.
    next: Option<(Consumer<f32>, u64)>,
    nbuf: Vec<f32>,
    garbage: Sender<OutGarbage>,
    shared: Arc<Shared>,
    sampler: Sampler,
    mix: Vec<f32>,
    play: Vec<f32>,
    pads: Vec<f32>,
    /// Monitoring direct des entrées joué par ce flux.
    monitor: Option<MonitorOut>,
    mon: Vec<f32>,
    /// Lecture interrompue par un trou : fondu d'entrée à la reprise.
    play_fade_in: bool,
    /// Flux d'écoute principal : c'est lui qui mesure la latence de sortie.
    main: bool,
    sample_rate: u32,
    /// Flux du moteur (`SLOT_*`) : battement de cœur et bit de perte.
    slot: usize,
    /// Panique rattrapée : le flux ne joue plus que du silence (il va être rouvert).
    faulted: bool,
}

/// Lit exactement `dst.len()` échantillons (disponibles) du tampon.
fn read_into(c: &mut Consumer<f32>, dst: &mut [f32]) {
    if dst.is_empty() {
        return;
    }
    if let Ok(chunk) = c.read_chunk(dst.len()) {
        let (a, b) = chunk.as_slices();
        dst[..a.len()].copy_from_slice(a);
        dst[a.len()..a.len() + b.len()].copy_from_slice(b);
        chunk.commit_all();
    }
}

/// Fondu de sortie sur les `fade` dernières trames de `buf[..n]` (stéréo entrelacé).
fn fade_tail(buf: &mut [f32], n: usize, fade: usize) {
    let k = fade.min(n);
    for i in 0..k {
        let g = (k - i) as f32 / (k + 1) as f32;
        let f = n - k + i;
        buf[f * 2] *= g;
        buf[f * 2 + 1] *= g;
    }
}

/// Fondu d'entrée sur les `fade` premières trames de `buf[..n]` (stéréo entrelacé).
fn fade_head(buf: &mut [f32], n: usize, fade: usize) {
    let k = fade.min(n);
    for i in 0..k {
        let g = (i + 1) as f32 / (k + 1) as f32;
        buf[i * 2] *= g;
        buf[i * 2 + 1] *= g;
    }
}

/// Réception du monitoring dans un flux de sortie.
struct MonitorOut {
    cons: Consumer<f32>,
    left: usize,
    /// Le tampon a assez d'avance pour jouer sans trou.
    primed: bool,
    /// Réserve moyenne (trames), lissée : sert à compenser la dérive des horloges.
    avg: f32,
    /// Fondu d'entrée à appliquer au prochain bloc (reprise après un trou).
    fade_in: bool,
    /// Bloc lu avant ré-échantillonnage (alloué hors du thread audio).
    tmp: Vec<f32>,
}

/// Fondu appliqué autour d'un trou de monitoring (trames).
const MON_FADE: usize = 64;

impl MonitorOut {
    fn new(cons: Consumer<f32>, left: usize) -> Self {
        Self {
            cons,
            left,
            primed: false,
            avg: 0.0,
            fade_in: false,
            tmp: vec![0.0; (MAX_BLOCK + 2) * 2],
        }
    }

    /// Remplit `out` (`frames` trames stéréo entrelacées) avec le monitoring.
    /// `in_block` = taille des paquets livrés par l'entrée.
    ///
    /// L'entrée et la sortie n'ont pas la même horloge et livrent par paquets, parfois
    /// par rafales : on garde une réserve d'environ deux paquets, et on ramène doucement
    /// la réserve vers cette cible en lisant un échantillon de plus ou de moins par bloc,
    /// ré-échantillonné (inaudible, 0,2 % au plus). Jamais de saut ni de coupure sèche.
    fn render(&mut self, out: &mut [f32], frames: usize, in_block: usize) {
        out.fill(0.0);
        if frames == 0 {
            return;
        }
        let cap = self.cons.buffer().capacity() / 2;
        let blk = frames.max(in_block).max(32);
        // Cible bornée au quart du tampon : toujours atteignable (sinon jamais amorcé).
        let target = (2 * blk + 64).min(cap / 4);
        let mut avail = self.cons.slots() / 2;
        // Énorme avance (après un blocage du système) : retour à la cible d'un coup,
        // avec un fondu d'entrée. Rare. Seuil toujours sous la capacité du tampon.
        let snap = (target * 4 + frames).min((target + cap) / 2);
        if avail > snap {
            let skip = avail - target;
            if let Ok(c) = self.cons.read_chunk(skip * 2) {
                c.commit_all();
            }
            avail = target;
            self.avg = target as f32;
            self.fade_in = true;
        }
        if !self.primed {
            if avail < target {
                return;
            }
            self.primed = true;
            self.fade_in = true;
            self.avg = avail as f32;
        }
        if avail < frames {
            // Trou inévitable : on joue la fin de la réserve en fondu de sortie, puis on
            // attend qu'elle se reconstitue (pas de coupure sèche, donc pas de clic).
            if let Ok(chunk) = self.cons.read_chunk(avail * 2) {
                let (a, b) = chunk.as_slices();
                out[..a.len()].copy_from_slice(a);
                out[a.len()..a.len() + b.len()].copy_from_slice(b);
                chunk.commit_all();
            }
            fade_tail(out, avail, MON_FADE);
            self.primed = false;
            return;
        }
        // Dérive : réserve moyenne au-dessus de la cible → lire un échantillon de plus ;
        // en dessous → un de moins. Zone morte d'un quart de paquet (pas d'oscillation).
        self.avg += (avail as f32 - self.avg) * 0.05;
        let dead = (blk / 4) as f32;
        let take = if self.avg > target as f32 + dead && avail > frames {
            frames + 1
        } else if self.avg < target as f32 - dead && frames > 2 {
            frames - 1
        } else {
            frames
        }
        .min(avail);
        let tmp = &mut self.tmp[..take * 2];
        if let Ok(chunk) = self.cons.read_chunk(take * 2) {
            let (a, b) = chunk.as_slices();
            tmp[..a.len()].copy_from_slice(a);
            tmp[a.len()..a.len() + b.len()].copy_from_slice(b);
            chunk.commit_all();
        }
        if take == frames {
            out[..frames * 2].copy_from_slice(tmp);
        } else {
            // Ré-échantillonnage linéaire de `take` trames vers `frames` trames.
            let ratio = (take - 1) as f32 / (frames - 1).max(1) as f32;
            for j in 0..frames {
                let pos = j as f32 * ratio;
                let i0 = (pos as usize).min(take - 1);
                let i1 = (i0 + 1).min(take - 1);
                let fr = pos - i0 as f32;
                for c in 0..2 {
                    out[j * 2 + c] = tmp[i0 * 2 + c] * (1.0 - fr) + tmp[i1 * 2 + c] * fr;
                }
            }
        }
        if self.fade_in {
            self.fade_in = false;
            fade_head(out, frames, MON_FADE);
        }
    }
}

impl OutState {
    fn new(
        channels: usize,
        left: usize,
        rx: Receiver<OutCmd>,
        garbage: Sender<OutGarbage>,
        shared: Arc<Shared>,
        sampler: Sampler,
    ) -> Self {
        Self {
            channels,
            left,
            rx,
            cons: None,
            next: None,
            nbuf: vec![0.0; MAX_BLOCK * 2],
            garbage,
            shared,
            sampler,
            mix: vec![0.0; MAX_BLOCK * channels],
            play: vec![0.0; MAX_BLOCK * 2],
            pads: vec![0.0; MAX_BLOCK * 2],
            monitor: None,
            mon: vec![0.0; MAX_BLOCK * 2],
            play_fade_in: false,
            main: false,
            sample_rate: 0,
            slot: SLOT_MAIN,
            faulted: false,
        }
    }

    /// Flux d'écoute principal (mesure de la latence de sortie à `sample_rate`).
    fn main_output(mut self, sample_rate: u32) -> Self {
        self.main = true;
        self.sample_rate = sample_rate;
        self
    }

    /// Flux du moteur que joue cet état (`SLOT_*`).
    fn for_slot(mut self, slot: usize) -> Self {
        self.slot = slot.min(SLOTS - 1);
        self
    }

    /// Ce flux joue aussi le monitoring, sur la paire `left`, `left + 1`.
    fn with_monitor(mut self, cons: Option<Consumer<f32>>, left: usize) -> Self {
        let last = self.channels.saturating_sub(1);
        self.monitor = cons.map(|cons| MonitorOut::new(cons, left.min(last)));
        self
    }

    /// Rend un objet à l'interface pour qu'elle le libère (jamais de libération ici).
    fn dispose(&self, g: OutGarbage) {
        if let Err(e) = self.garbage.try_send(g) {
            std::mem::forget(e.into_inner());
        }
    }

    fn release(&mut self) {
        if let Some(c) = self.cons.take() {
            // Le ring buffer est libéré par l'interface, jamais par le thread audio.
            self.dispose(OutGarbage::Play(c));
        }
        self.release_next();
    }

    fn release_next(&mut self) {
        if let Some((c, _)) = self.next.take() {
            self.dispose(OutGarbage::Play(c));
        }
    }

    /// Callback de sortie complet. `ts` = (callback, restitution) d'après le pilote. Une
    /// panique (cas non prévu) ne remonte jamais dans le pilote (en ASIO, elle fermerait
    /// l'application, prises comprises) : le flux ne joue plus que du silence et est
    /// signalé perdu, pour être rouvert. Aucun coût ni allocation sur le chemin normal.
    fn callback<T>(
        &mut self,
        data: &mut [T],
        ts: Option<(cpal::StreamInstant, cpal::StreamInstant)>,
    ) where
        T: SizedSample + FromSample<f32>,
    {
        self.shared.heartbeat[self.slot].fetch_add(1, Ordering::Relaxed);
        if self.faulted {
            data.fill(T::EQUILIBRIUM);
            return;
        }
        let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.render_into(&mut *data, ts)
        }))
        .is_ok();
        if !ok {
            self.faulted = true;
            data.fill(T::EQUILIBRIUM);
            self.shared.fault(slot_bit(self.slot));
        }
    }

    /// Commandes, horodatage, puis mix de tout le bloc converti au format de la carte.
    fn render_into<T>(
        &mut self,
        data: &mut [T],
        ts: Option<(cpal::StreamInstant, cpal::StreamInstant)>,
    ) where
        T: SizedSample + FromSample<f32>,
    {
        let st = self;
        if st.main
            && let Some((callback, playback)) = ts
        {
            // Latence callback → restitution (compensation des prises en re-recording).
            st.shared.out_latency.store(
                to_frames(playback.duration_since(callback), st.sample_rate),
                Ordering::Relaxed,
            );
        }
        while let Ok(cmd) = st.rx.try_recv() {
            match cmd {
                OutCmd::Switch(c, at) => {
                    st.release_next();
                    st.next = Some((c, at));
                }
                OutCmd::Play(c) => {
                    st.release();
                    st.cons = Some(c);
                    st.play_fade_in = false;
                    // Remis à zéro ici, au moment exact où la nouvelle lecture commence.
                    st.shared.out_frames.store(0, Ordering::Relaxed);
                }
                OutCmd::Stop => st.release(),
                OutCmd::Monitor(m) => {
                    if let Some(old) = st.monitor.replace(m) {
                        st.dispose(OutGarbage::Monitor(old));
                    }
                }
            }
        }
        let ch = st.channels;
        if ch == 0 {
            return;
        }
        let total = data.len() / ch;
        st.shared.out_block.store(total as u32, Ordering::Relaxed);
        // Horodatage du début de ce bloc (lecture en cours seulement), par la sortie
        // principale uniquement : celles du soundboard ou de l'écoute ne lisent jamais
        // la lecture multipiste et ne doivent pas l'effacer.
        if st.main {
            let sh = &st.shared;
            if st.cons.is_some() {
                sh.out_cb_frames
                    .store(sh.out_frames.load(Ordering::Relaxed), Ordering::Relaxed);
                sh.out_cb_ns.store(sh.now_ns(), Ordering::Release);
            } else {
                sh.out_cb_ns.store(0, Ordering::Release);
            }
        }
        let mut done = 0;
        while done < total {
            let n = (total - done).min(MAX_BLOCK);
            st.process(n);
            let out = &mut data[done * ch..(done + n) * ch];
            for (d, &s) in out.iter_mut().zip(&st.mix[..n * ch]) {
                // Jamais de NaN / infini vers la carte (sécurité des enceintes), et
                // écrêtage à la pleine échelle : la conversion f32 → 24 bits ne sature
                // pas, un dépassement y reviendrait en craquement pleine échelle.
                *d = T::from_sample(out_sample(s));
            }
            done += n;
        }
        // Bloc incomplet (taille non multiple des canaux) : jamais de reste non écrit.
        data[total * ch..].fill(T::EQUILIBRIUM);
    }

    /// Bascule vers le rendu suivant si sa trame de départ tombe dans ce bloc (ou est
    /// passée) et qu'il a déjà assez d'avance : l'ancien rendu joue jusqu'à la bascule,
    /// puis fondu enchaîné court vers le nouveau (pas de clic). `true` si le bloc a été
    /// joué ainsi. Sinon, l'ancien rendu continue (il a des secondes d'avance) et on
    /// réessaie au bloc suivant, toujours calé sur la même trame.
    fn try_switch(&mut self, frames: usize) -> bool {
        let played = self.shared.out_frames.load(Ordering::Relaxed);
        let Some((ncons, at)) = self.next.as_ref() else {
            return false;
        };
        let at = *at;
        if at >= played + frames as u64 {
            return false;
        }
        let k = at.saturating_sub(played) as usize;
        let late = played.saturating_sub(at) as usize;
        let want = frames - k;
        if ncons.slots() / 2 < late + want {
            return false;
        }
        let (mut ncons, _) = self.next.take().unwrap();
        let play = &mut self.play[..frames * 2];
        play.fill(0.0);
        let xf = MON_FADE.min(want);
        let mut old_n = 0;
        if let Some(c) = self.cons.as_mut() {
            old_n = (k + xf).min(c.slots() / 2);
            read_into(c, &mut play[..old_n * 2]);
        }
        if late > 0
            && let Ok(ch) = ncons.read_chunk(late * 2)
        {
            ch.commit_all();
        }
        let nb = &mut self.nbuf[..want * 2];
        read_into(&mut ncons, nb);
        for i in 0..want {
            let g = if i < xf {
                (i + 1) as f32 / (xf + 1) as f32
            } else {
                1.0
            };
            for c in 0..2 {
                let idx = (k + i) * 2 + c;
                let old = if k + i < old_n { play[idx] } else { 0.0 };
                play[idx] = old * (1.0 - g) + nb[i * 2 + c] * g;
            }
        }
        if let Some(c) = self.cons.take() {
            self.dispose(OutGarbage::Play(c));
        }
        self.cons = Some(ncons);
        self.play_fade_in = false;
        self.shared
            .out_frames
            .fetch_add(frames as u64, Ordering::Relaxed);
        true
    }

    /// Calcule `frames` trames (≤ MAX_BLOCK) dans `self.mix`.
    fn process(&mut self, frames: usize) {
        let ch = self.channels;
        let switched = self.try_switch(frames);
        let sh = &*self.shared;
        let play = &mut self.play[..frames * 2];
        if !switched {
            play.fill(0.0);
        }
        if switched {
        } else if let Some(cons) = self.cons.as_mut() {
            let n = frames.min(cons.slots() / 2);
            if n < frames && !cons.is_abandoned() {
                sh.underruns.fetch_add(1, Ordering::Relaxed);
            }
            if let Ok(chunk) = cons.read_chunk(n * 2) {
                let (a, b) = chunk.as_slices();
                play[..a.len()].copy_from_slice(a);
                play[a.len()..a.len() + b.len()].copy_from_slice(b);
                chunk.commit_all();
            }
            // Trou de lecture (ou imminent : le prochain bloc ne tiendra pas) : fondus
            // courts de part et d'autre, pas de marche brutale donc pas de clic. Avec 3 s de
            // tampon, « imminent » ne se produit qu'au bord d'un vrai trou.
            if n > 0 && self.play_fade_in {
                self.play_fade_in = false;
                fade_head(play, n, MON_FADE);
            }
            if n < frames || cons.slots() / 2 < frames {
                fade_tail(play, n, MON_FADE);
                self.play_fade_in = true;
            }
            sh.out_frames.fetch_add(n as u64, Ordering::Relaxed);
        }
        let pads = &mut self.pads[..frames * 2];
        pads.fill(0.0);
        self.sampler.render(pads, frames, &sh.pad_pos);

        let mix = &mut self.mix[..frames * ch];
        mix.fill(0.0);
        let route = |mix: &mut [f32], src: &[f32], left: usize, meter: &[AtomicU32; 2]| {
            let right = (left + 1).min(ch - 1);
            let (mut pl, mut pr) = (0f32, 0f32);
            for f in 0..frames {
                let (l, r) = (src[f * 2], src[f * 2 + 1]);
                pl = pl.max(l.abs());
                pr = pr.max(r.abs());
                if left == right {
                    mix[f * ch + left] += 0.5 * (l + r);
                } else {
                    mix[f * ch + left] += l;
                    mix[f * ch + right] += r;
                }
            }
            meter[0].fetch_max(pl.to_bits(), Ordering::Relaxed);
            meter[1].fetch_max(pr.to_bits(), Ordering::Relaxed);
        };
        route(mix, play, self.left, &sh.master_play);
        route(mix, pads, self.sampler.left, &sh.master_pad);

        // Monitoring direct : latence courte, sans trous ni sauts (voir MonitorOut).
        if let Some(m) = self.monitor.as_mut() {
            let in_block = sh.in_block.load(Ordering::Relaxed) as usize;
            m.render(&mut self.mon[..frames * 2], frames, in_block);
            let left = m.left;
            let mon = &self.mon[..frames * 2];
            route(mix, mon, left, &sh.master_mon);
        }
    }
}

/// Ouvre et démarre un flux de sortie. `state` reçoit le nombre de canaux retenu.
fn start_output(
    dev: &cpal::Device,
    sample_rate: u32,
    buffer: cpal::BufferSize,
    state: impl FnOnce(u16) -> OutState,
) -> Result<(cpal::Stream, u16)> {
    let (fmt, channels) = pick_config(dev, sample_rate, false)?;
    let cfg = StreamConfig {
        channels,
        sample_rate,
        buffer_size: buffer,
    };
    let st = state(channels);
    let s = match fmt {
        SampleFormat::F32 => build_output::<f32>(dev, cfg, st),
        SampleFormat::I32 => build_output::<i32>(dev, cfg, st),
        SampleFormat::I24 => build_output::<cpal::I24>(dev, cfg, st),
        SampleFormat::I16 => build_output::<i16>(dev, cfg, st),
        f => Err(anyhow!("Format de sortie non géré : {f:?}")),
    }?;
    s.play().context("Démarrage du flux de sortie")?;
    Ok((s, channels))
}

/// Échantillon envoyé à la carte : fini et dans [-1, 1[ (aucun format ne déborde).
#[inline]
fn out_sample(s: f32) -> f32 {
    if s.is_finite() {
        s.clamp(-1.0, 1.0 - 1.0 / 8_388_608.0)
    } else {
        0.0
    }
}

fn build_output<T>(dev: &cpal::Device, cfg: StreamConfig, mut st: OutState) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32> + Send + 'static,
{
    let err_shared = st.shared.clone();
    // Chaque flux signale sa propre perte (bit de son `SLOT_*`), quoi qu'il joue
    // (soundboard, monitoring) : l'interface sait ainsi lequel rouvrir.
    let bit = slot_bit(st.slot);
    let stream = dev.build_output_stream::<T, _, _>(
        cfg,
        move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
            let ts = info.timestamp();
            st.callback(data, Some((ts.callback, ts.playback)));
        },
        move |e| err_shared.on_error(e, bit),
        Some(OPEN_TIMEOUT),
    )?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    /// Monitoring : entrée mono au centre à -3 dB ; beaucoup d'entrées écoutées ensemble
    /// ne dépassent jamais -1 dBFS (limiteur), sans toucher aux prises.
    #[test]
    fn monitor_mix_pan_law_and_limiter() {
        let run = |listened: usize, level: f32| -> Vec<f32> {
            let shared = Arc::new(Shared::new());
            shared.monitor_on.store(true, Ordering::Relaxed);
            for c in 0..listened {
                shared.monitor[c].store(1, Ordering::Relaxed);
            }
            let (mprod, mut mcons) = rtrb::RingBuffer::new(1 << 16);
            let mut st = InState {
                channels: 8,
                rx: bounded::<InCmd>(1).1,
                targets: Vec::new(),
                garbage: bounded(1).0,
                shared,
                monitor: Some(mprod),
                sample_rate: 48000,
                last_capture: None,
                mon_env: 0.0,
                mon_release: mon_release(48000),
                faulted: false,
            };
            st.process(&vec![level; 512 * 8]);
            let mut out = Vec::new();
            while let Ok(x) = mcons.pop() {
                out.push(x);
            }
            out
        };
        let one = run(1, 0.5);
        assert!(
            one.iter()
                .all(|&x| (x - 0.5 * std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6)
        );
        let eight = run(8, 0.9);
        assert!(eight.iter().all(|&x| x <= 0.8911), "{:?}", &eight[..4]);
        assert!(eight.iter().all(|&x| x > 0.88));
    }

    /// Décrochage détecté par l'horodatage de capture, sans fausse alerte sur des blocs
    /// livrés en rafale.
    #[test]
    fn capture_gap_counts_dropouts_only() {
        let shared = Arc::new(Shared::new());
        shared.counting.store(true, Ordering::Relaxed);
        let mut st = InState {
            channels: 2,
            rx: bounded::<InCmd>(1).1,
            targets: Vec::new(),
            garbage: bounded(1).0,
            shared: shared.clone(),
            monitor: None,
            sample_rate: 48000,
            last_capture: None,
            mon_env: 0.0,
            mon_release: mon_release(48000),
            faulted: false,
        };
        let t0 = cpal::StreamInstant::ZERO;
        let at = |frames: u64| {
            t0.checked_add(std::time::Duration::from_nanos(
                frames * 1_000_000_000 / 48000,
            ))
            .unwrap()
        };
        // Blocs réguliers de 480 trames : rien.
        for k in 0..10 {
            st.check_capture_gap(at(k * 480), 480);
        }
        assert_eq!(shared.xruns.load(Ordering::Relaxed), 0);
        // Petit retard d'horloge (5 ms) : pas d'alerte.
        st.check_capture_gap(at(10 * 480 + 240), 480);
        assert_eq!(shared.xruns.load(Ordering::Relaxed), 0);
        // 1920 trames manquantes (40 ms) : un décrochage.
        st.check_capture_gap(at(11 * 480 + 240 + 1920), 480);
        assert_eq!(shared.xruns.load(Ordering::Relaxed), 1);
    }

    /// Règle n°1 du temps réel (R. Bencina, T. Doumler) : aucune allocation ni
    /// libération dans les callbacks. Chaque chemin est exercé par les callbacks complets
    /// (ceux qu'appelle le pilote) : prise mono et stéréo, monitoring, arrêt de prise, trou
    /// de l'entrée comblé par du silence, nouveau tampon de monitoring, lecture, bascule
    /// mute/solo effective, arrêt, pads (appui, fondu, panique), monitoring en sortie
    /// (amorçage, dérive, trou, rattrapage), trames perdues, formats entiers — le test
    /// échoue à la moindre allocation.
    #[test]
    #[cfg(debug_assertions)]
    fn audio_callbacks_never_allocate() {
        use crate::sampler::{PadMode, PadParams, SampleData};
        use assert_no_alloc::assert_no_alloc;
        let at = |frames: u64| {
            cpal::StreamInstant::ZERO
                .checked_add(std::time::Duration::from_nanos(
                    frames * 1_000_000_000 / 48000,
                ))
                .unwrap()
        };
        // --- entrée : 8 canaux, une cible mono, une stéréo, monitoring de 2 canaux.
        let shared = Arc::new(Shared::new());
        shared.monitor_on.store(true, Ordering::Relaxed);
        shared.monitor[0].store(1, Ordering::Relaxed);
        shared.monitor[3].store(2, Ordering::Relaxed);
        let (tx, rx) = bounded::<InCmd>(4);
        let (gtx, _grx) = bounded::<InGarbage>(16);
        let (mprod, _mcons) = rtrb::RingBuffer::new(48000);
        let mut ins = InState {
            channels: 8,
            rx,
            targets: Vec::new(),
            garbage: gtx,
            shared: shared.clone(),
            monitor: Some(mprod),
            sample_rate: 48000,
            last_capture: None,
            mon_env: 0.0,
            mon_release: mon_release(48000),
            faulted: false,
        };
        let (p1, _c1) = rtrb::RingBuffer::new(4096);
        let (p2, _c2) = rtrb::RingBuffer::new(1 << 20);
        // Copie de sécurité sur la piste stéréo, avec un petit tampon (pertes comprises).
        let (p3, _c3) = rtrb::RingBuffer::new(4096);
        tx.send(InCmd::Record(vec![
            RecTarget::new(1, p1),
            RecTarget::stereo(4, p2).with_copy(Some(p3)),
        ]))
        .unwrap();
        let data_f = vec![0.25f32; 256 * 8];
        let data_i = vec![1 << 20; 256 * 8];
        let mut pos = 0u64;
        assert_no_alloc(|| {
            // Le petit tampon mono déborde vite : chemin « trames perdues » compris.
            for _ in 0..40 {
                ins.callback(&data_f, Some((at(pos), at(pos))));
                pos += 256;
                ins.callback::<i32>(&data_i, Some((at(pos), at(pos))));
                pos += 256;
            }
        });
        tx.send(InCmd::Stop).unwrap();
        assert_no_alloc(|| ins.callback(&data_f, None));
        // Nouvelle prise sur de grands tampons, avec un trou d'une demi-seconde (horloge
        // perdue) : silence inséré. Puis nouveau tampon de monitoring (sortie rouverte).
        let (p4, _c4) = rtrb::RingBuffer::new(1 << 20);
        let (p5, _c5) = rtrb::RingBuffer::new(1 << 20);
        tx.send(InCmd::Record(vec![
            RecTarget::new(0, p4),
            RecTarget::stereo(2, p5),
        ]))
        .unwrap();
        let (mprod2, _mcons2) = rtrb::RingBuffer::new(48000);
        assert_no_alloc(|| {
            for k in 0..20 {
                if k == 10 {
                    pos += 24000;
                }
                ins.callback(&data_f, Some((at(pos), at(pos))));
                pos += 256;
            }
        });
        assert!(shared.rec_gap_frames.load(Ordering::Relaxed) > 20000);
        tx.send(InCmd::Monitor(mprod2)).unwrap();
        assert_no_alloc(|| ins.callback(&data_f, Some((at(pos), at(pos)))));
        assert!(ins.monitor.is_some());

        // --- sortie : lecture, bascule, pads, nouveau monitoring.
        let (otx, orx) = bounded::<OutCmd>(8);
        let (ogt, _ogr) = bounded(16);
        let (ptx, prx) = bounded::<PadCmd>(64);
        let (sgt, _sgr) = bounded(64);
        let mut out = OutState::new(
            2,
            0,
            orx,
            ogt,
            shared.clone(),
            Sampler::new(prx, sgt, 0, 48000),
        )
        .main_output(48000);
        let (mut pp, pc) = rtrb::RingBuffer::new(1 << 16);
        let (mut pp2, pc2) = rtrb::RingBuffer::new(1 << 16);
        for _ in 0..(1 << 15) {
            let _ = pp.push(0.1);
            let _ = pp2.push(0.2);
        }
        otx.send(OutCmd::Play(pc)).unwrap();
        otx.send(OutCmd::Switch(pc2, 2048)).unwrap();
        let smp = Arc::new(SampleData {
            sample_rate: 48000,
            frames: 48000,
            data: vec![0.3; 96000],
            peaks: Vec::new(),
        });
        let params = |mode| PadParams {
            start: 0,
            end: 48000,
            fade_in: 100,
            fade_out: 100,
            gain: 1.0,
            mode,
            looped: true,
        };
        for (slot, mode) in [
            (0, PadMode::Toggle),
            (1, PadMode::Gate),
            (2, PadMode::Retrigger),
        ] {
            ptx.send(PadCmd::Press {
                slot,
                sample: smp.clone(),
                params: params(mode),
            })
            .unwrap();
        }
        let mut buf_f = vec![0f32; 128 * 2];
        let mut buf_i = vec![0i32; 128 * 2];
        let ts = Some((at(0), at(480)));
        assert_no_alloc(|| {
            for _ in 0..25 {
                out.callback(&mut buf_f, ts);
                out.callback::<i32>(&mut buf_i, ts);
            }
        });
        assert!(out.next.is_none() && out.cons.is_some(), "bascule faite");
        assert!(buf_f.iter().any(|&x| x != 0.0));
        // Nouveau tampon de monitoring, deux fois (l'ancien part à la poubelle).
        for _ in 0..2 {
            let (_mp, mc) = rtrb::RingBuffer::new(MON_RING * 2);
            otx.send(OutCmd::Monitor(MonitorOut::new(mc, 0))).unwrap();
            assert_no_alloc(|| out.callback(&mut buf_f, ts));
        }
        ptx.send(PadCmd::Release { slot: 1 }).unwrap();
        ptx.send(PadCmd::FadeAll { frames: 4800 }).unwrap();
        otx.send(OutCmd::Stop).unwrap();
        assert_no_alloc(|| {
            for _ in 0..200 {
                out.callback(&mut buf_f, ts);
            }
        });
        ptx.send(PadCmd::StopAll).unwrap();
        assert_no_alloc(|| out.callback(&mut buf_f, ts));

        // --- monitoring joué par une sortie : amorçage, dérive (ré-échantillonnage), trou,
        // énorme avance (rattrapage).
        let (mut mst, mut mp) = monitor_state(2, 0);
        let mut mbuf = vec![0f32; 128 * 2];
        let push = |mp: &mut Producer<f32>, frames: usize| {
            for _ in 0..frames * 2 {
                let _ = mp.push(0.5);
            }
        };
        assert_no_alloc(|| {
            push(&mut mp, 100);
            mst.callback(&mut mbuf, None);
            push(&mut mp, 400);
            mst.callback(&mut mbuf, None);
            for _ in 0..60 {
                push(&mut mp, 131);
                mst.callback(&mut mbuf, None);
            }
            for _ in 0..6 {
                mst.callback(&mut mbuf, None);
            }
            push(&mut mp, 30000);
            mst.callback(&mut mbuf, None);
            push(&mut mp, 128);
            mst.callback(&mut mbuf, None);
        });
        assert!(mbuf.iter().any(|&x| x != 0.0));
    }

    /// Une panique dans un callback (cas non prévu) ne remonte jamais dans le pilote : le
    /// flux joue du silence, est signalé perdu (pour être rouvert) et le reste ignoré.
    #[test]
    fn callback_panic_is_contained() {
        let shared = Arc::new(Shared::new());
        let (gt, _gr) = bounded(1);
        let (sgt, _sgr) = bounded(1);
        let mut st = OutState::new(
            2,
            0,
            bounded::<OutCmd>(1).1,
            gt,
            shared.clone(),
            Sampler::new(bounded::<PadCmd>(1).1, sgt, 0, 48000),
        )
        .for_slot(SLOT_PADS);
        // Paire hors du périphérique (impossible en vrai, `left` est borné à l'ouverture) :
        // indice hors limites dans le mix, donc panique.
        st.left = 7;
        let mut data = vec![0.7f32; 64 * 2];
        st.callback(&mut data, None);
        assert!(st.faulted);
        assert!(data.iter().all(|&x| x == 0.0), "silence après la panique");
        assert_eq!(shared.device_lost.load(Ordering::Acquire), LOST_PADS);
        assert!(shared.error_text().is_some_and(|t| t.contains("rouvert")));
        // Ensuite : silence, plus aucun traitement, mais le battement de cœur continue.
        data.fill(0.7);
        st.callback(&mut data, None);
        assert!(data.iter().all(|&x| x == 0.0));
        assert_eq!(shared.heartbeat[SLOT_PADS].load(Ordering::Relaxed), 2);
    }

    /// Trou de l'entrée pendant une prise (horloge Dante perdue une demi-seconde) : autant
    /// de silence est inséré, la suite reste à l'heure ; un simple retard d'horloge (petit
    /// écart) ne touche à rien.
    #[test]
    fn input_gap_is_filled_with_silence() {
        let shared = Arc::new(Shared::new());
        let (tx, rx) = bounded::<InCmd>(4);
        let mut st = InState {
            channels: 1,
            rx,
            targets: Vec::new(),
            garbage: bounded(4).0,
            shared: shared.clone(),
            monitor: None,
            sample_rate: 48000,
            last_capture: None,
            mon_env: 0.0,
            mon_release: mon_release(48000),
            faulted: false,
        };
        let (prod, mut cons) = rtrb::RingBuffer::new(1 << 18);
        tx.send(InCmd::Record(vec![RecTarget::new(0, prod)]))
            .unwrap();
        let at = |frames: u64| {
            cpal::StreamInstant::ZERO
                .checked_add(std::time::Duration::from_nanos(
                    frames * 1_000_000_000 / 48000,
                ))
                .unwrap()
        };
        let data = vec![0.5f32; 480];
        let mut pos = 0;
        for k in 0..10 {
            // Petit retard d'horloge (100 ms) au 4e bloc : pas de silence.
            if k == 4 {
                pos += 4800;
            }
            if k == 7 {
                pos += 24000;
            }
            st.callback(&data, Some((at(pos), at(pos))));
            pos += 480;
        }
        let mut got = Vec::new();
        while let Ok(x) = cons.pop() {
            got.push(x);
        }
        assert_eq!(shared.rec_gap_frames.load(Ordering::Relaxed), 24000);
        assert_eq!(got.len(), 10 * 480 + 24000);
        assert_eq!(shared.rec_frames.load(Ordering::Relaxed), got.len() as u64);
        assert!(got[..7 * 480].iter().all(|&x| x == 0.5));
        assert!(got[7 * 480..7 * 480 + 24000].iter().all(|&x| x == 0.0));
        assert!(got[7 * 480 + 24000..].iter().all(|&x| x == 0.5));
    }

    /// Une carte rebranchée sur un autre port USB revient sous un autre nom (« 2- … ») :
    /// retrouvée, sauf ambiguïté ; l'identité stable passe avant le nom.
    #[test]
    fn devices_are_found_again_after_renaming() {
        assert_eq!(
            normalize_device_name("Haut-parleurs (2- Focusrite USB Audio)"),
            normalize_device_name("Haut-parleurs (Focusrite USB Audio)")
        );
        assert_eq!(
            normalize_device_name("12- Line (RME)"),
            normalize_device_name("line (rme)")
        );
        // « 1-2 » fait partie du nom (paire de canaux) : jamais retiré.
        assert_ne!(
            normalize_device_name("Line 1-2 (Focusrite)"),
            normalize_device_name("Line 2 (Focusrite)")
        );
        let c = |v: &[(&str, &str)]| -> Vec<(Option<String>, String)> {
            v.iter()
                .map(|(i, n)| (Some(i.to_string()), n.to_string()))
                .collect()
        };
        let devs = c(&[
            ("a", "Haut-parleurs (Realtek)"),
            ("b", "Haut-parleurs (2- Focusrite USB Audio)"),
        ]);
        let want = Some("Haut-parleurs (Focusrite USB Audio)");
        assert_eq!(match_device(&devs, want, None), Some((1, true)));
        assert_eq!(
            match_device(&devs, Some("Haut-parleurs (Realtek)"), None),
            Some((0, false))
        );
        // Identité d'abord, même si le nom a changé.
        assert_eq!(match_device(&devs, want, Some("b")), Some((1, true)));
        assert_eq!(
            match_device(&devs, Some("Haut-parleurs (Realtek)"), Some("a")),
            Some((0, false))
        );
        // Identité inconnue (carte remplacée) : repli sur le nom.
        assert_eq!(match_device(&devs, want, Some("zz")), Some((1, true)));
        // Deux cartes identiques : aucune n'est choisie au hasard.
        let twins = c(&[
            ("a", "Line (2- Focusrite USB Audio)"),
            ("b", "Line (3- Focusrite USB Audio)"),
        ]);
        assert_eq!(
            match_device(&twins, Some("Line (Focusrite USB Audio)"), None),
            None
        );
        assert_eq!(
            match_device(&twins, Some("Line (3- Focusrite USB Audio)"), None),
            Some((1, false))
        );
        assert_eq!(match_device(&devs, Some("Absente"), None), None);
    }

    #[test]
    fn play_position_is_interpolated_between_output_blocks() {
        let sh = super::Shared::new();
        std::thread::sleep(std::time::Duration::from_millis(30));
        // Pas de lecture horodatée : on garde le compteur tel quel.
        sh.out_frames.store(1024, Ordering::Relaxed);
        assert_eq!(sh.play_position_now(48000), 1024);
        // Bloc [1024, 2048[ demandé il y a ~10 ms : position ≈ 1024 + 480.
        sh.out_cb_frames.store(1024, Ordering::Relaxed);
        sh.out_frames.store(2048, Ordering::Relaxed);
        let t = sh.now_ns().saturating_sub(10_000_000).max(1);
        sh.out_cb_ns.store(t, Ordering::Release);
        let p = sh.play_position_now(48000);
        assert!((1024 + 470..1024 + 600).contains(&p), "{p}");
        // Jamais au-delà de ce que le pilote a demandé.
        sh.out_cb_ns.store(1, Ordering::Release);
        assert_eq!(sh.play_position_now(48000), 2048);
        // Compteur remis à zéro (nouvelle lecture) : pas d'interpolation sur l'ancien bloc.
        sh.out_frames.store(0, Ordering::Relaxed);
        assert_eq!(sh.play_position_now(48000), 0);
    }

    #[test]
    fn output_never_wraps_on_24_bit_cards() {
        use cpal::Sample;
        for s in [1.0f32, 1.5, 3.0, 1e9] {
            let v = cpal::I24::from_sample(super::out_sample(s)).to_sample::<f32>();
            assert!(v > 0.99, "{s} → {v}");
            let v = cpal::I24::from_sample(super::out_sample(-s)).to_sample::<f32>();
            assert!(v < -0.99, "-{s} → {v}");
        }
        assert_eq!(super::out_sample(f32::NAN), 0.0);
        assert_eq!(super::out_sample(f32::INFINITY), 0.0);
        assert_eq!(i16::from_sample(super::out_sample(2.0)), i16::MAX);
    }

    use super::*;

    /// Après une saturation, la piste reçoit du silence à la place des trames perdues :
    /// la suite reste à la bonne position (synchronisation entre pistes préservée).
    #[test]
    fn overrun_keeps_alignment() {
        let (prod, mut cons) = rtrb::RingBuffer::new(100);
        let mut t = RecTarget::new(0, prod);
        assert!(!feed_target(&mut t, 80, |_, _| 1.0));
        // Seules 20 trames tiennent : 30 sont perdues.
        assert!(feed_target(&mut t, 50, |_, _| 2.0));
        assert_eq!(t.pending, 30);
        let mut got: Vec<f32> = Vec::new();
        while let Ok(x) = cons.pop() {
            got.push(x);
        }
        assert!(!feed_target(&mut t, 40, |_, _| 3.0));
        while let Ok(x) = cons.pop() {
            got.push(x);
        }
        // 80 + 50 + 40 trames au total, la dernière série au bon endroit.
        assert_eq!(got.len(), 170);
        assert!(got[..80].iter().all(|&x| x == 1.0));
        assert!(got[80..100].iter().all(|&x| x == 2.0));
        assert!(got[100..130].iter().all(|&x| x == 0.0));
        assert!(got[130..].iter().all(|&x| x == 3.0));
    }

    fn monitor_state(channels: usize, left: usize) -> (OutState, Producer<f32>) {
        let (prod, cons) = rtrb::RingBuffer::new(MON_RING * 2);
        let (gt, _gr) = bounded(1);
        let (sgt, _sgr) = bounded(1);
        let st = OutState::new(
            channels,
            0,
            bounded::<OutCmd>(4).1,
            gt,
            Arc::new(Shared::new()),
            Sampler::new(bounded::<PadCmd>(1).1, sgt, 0, 48000),
        )
        .with_monitor(Some(cons), left);
        (st, prod)
    }

    /// Monitoring : le mix stéréo arrive sur la paire choisie, sans trou au démarrage.
    #[test]
    fn monitor_routes_to_its_pair() {
        let (mut st, mut prod) = monitor_state(4, 2);
        // Pas encore assez de réserve : silence.
        for _ in 0..100 {
            prod.push(0.5).unwrap();
            prod.push(-0.25).unwrap();
        }
        st.process(128);
        assert!(st.mix[..128 * 4].iter().all(|&x| x == 0.0));
        for _ in 0..400 {
            prod.push(0.5).unwrap();
            prod.push(-0.25).unwrap();
        }
        st.process(128);
        // Après le fondu d'entrée, le signal est intact, sur les sorties 3-4.
        let f: Vec<&[f32]> = st.mix[..128 * 4].chunks(4).collect();
        assert!(
            f[MON_FADE..].iter().all(|fr| fr == &[0.0, 0.0, 0.5, -0.25]),
            "{:?}",
            f[100]
        );
    }

    /// Simulation : l'entrée livre des paquets de 480 trames, parfois par rafales, et son
    /// horloge dérive par rapport à la sortie (dans un sens puis dans l'autre). Aucun trou,
    /// aucun saut, et une latence qui reste bornée.
    #[test]
    fn monitor_survives_jitter_and_drift() {
        for drift in [-1i64, 1] {
            let mut m = {
                let (prod, cons) = rtrb::RingBuffer::new(MON_RING * 2);
                (MonitorOut::new(cons, 0), prod)
            };
            let (mon, prod) = (&mut m.0, &mut m.1);
            let mut out = vec![0.0f32; 480 * 2];
            let mut next = 0u64; // valeur du prochain échantillon (rampe)
            let mut played = Vec::new();
            for call in 0..3000u64 {
                // Rafales : deux paquets d'un coup un appel sur trois, rien au suivant.
                let packets = match call % 3 {
                    0 => 2,
                    1 => 0,
                    _ => 1,
                };
                for _ in 0..packets {
                    // Dérive : un échantillon de plus (ou de moins) tous les 20 paquets.
                    let extra = if call % 20 == 0 { drift } else { 0 };
                    for _ in 0..(480 + extra) {
                        // Rampe de 0,1 à 1,1, jamais nulle : un zéro joué serait un trou.
                        let v = 0.1 + (next % 100_000) as f32 * 1e-5;
                        prod.push(v).unwrap();
                        prod.push(v).unwrap();
                        next += 1;
                    }
                }
                mon.render(&mut out, 480, 480);
                if call > 20 {
                    played.extend(out.iter().step_by(2).copied());
                    assert!(mon.cons.slots() / 2 < 4 * 1024, "latence qui grandit");
                }
            }
            // Pas de trou : jamais de zéro une fois lancé.
            let holes = played.iter().filter(|&&v| v == 0.0).count();
            assert!(holes == 0, "dérive {drift} : {holes} trous");
            // Pas de saut : des échantillons perdus feraient avancer la rampe d'un coup.
            // (Les retours au début de la rampe, vers le bas, ne comptent pas.)
            let jumps = played.windows(2).filter(|w| w[1] - w[0] > 3e-5).count();
            assert!(jumps == 0, "dérive {drift} : {jumps} sauts");
        }
    }

    /// Gros buffers (2048 et 8192 trames) : le monitoring s'amorce, et après un blocage
    /// (rafale qui remplit le tampon) la latence revient vite près de la cible.
    #[test]
    fn monitor_big_blocks_prime_and_recover() {
        for block in [2048usize, 8192] {
            let (prod, cons) = rtrb::RingBuffer::new(MON_RING * 2);
            let mut mon = MonitorOut::new(cons, 0);
            let mut prod = prod;
            let out_frames = block.min(MAX_BLOCK);
            let calls = block / out_frames;
            let mut out = vec![0.0f32; out_frames * 2];
            let push = |prod: &mut Producer<f32>| {
                let n = block.min(prod.slots() / 2);
                if let Ok(c) = prod.write_chunk_uninit(n * 2) {
                    c.fill_from_iter(std::iter::repeat_n(0.5f32, n * 2));
                }
                n
            };
            let target = (2 * block + 64).min(MON_RING / 4);
            let mut heard = false;
            for _ in 0..20 {
                push(&mut prod);
                for _ in 0..calls {
                    mon.render(&mut out, out_frames, block);
                    heard |= out.iter().any(|&x| x != 0.0);
                }
            }
            assert!(heard && mon.primed, "bloc {block} : jamais amorcé");
            // Blocage de la sortie : l'entrée continue jusqu'à remplir le tampon.
            while push(&mut prod) > 0 {}
            assert!(mon.cons.slots() / 2 > target * 2);
            for _ in 0..2 {
                push(&mut prod);
                for _ in 0..calls {
                    mon.render(&mut out, out_frames, block);
                }
            }
            let lat = mon.cons.slots() / 2;
            assert!(
                lat <= target + block,
                "bloc {block} : latence {lat} trames, cible {target}"
            );
            assert!(out.iter().all(|&x| x != 0.0), "bloc {block} : trou");
        }
    }

    /// Trou de lecture : fondu de sortie avant le trou, fondu d'entrée à la reprise.
    #[test]
    fn playback_underrun_fades() {
        let (mut st, _p) = monitor_state(2, 0);
        st.monitor = None;
        let (mut prod, cons) = rtrb::RingBuffer::new(4096);
        st.cons = Some(cons);
        let feed = |prod: &mut Producer<f32>, frames: usize| {
            for _ in 0..frames * 2 {
                prod.push(0.5).unwrap();
            }
        };
        feed(&mut prod, 128);
        st.process(128);
        assert_eq!(st.mix[0], 0.5);
        assert!(st.mix[127 * 2] < 0.02, "fin de bloc avant le trou : fondu");
        st.process(128);
        assert!(st.mix[..256].iter().all(|&x| x == 0.0));
        feed(&mut prod, 256);
        st.process(128);
        assert!(st.mix[0] < 0.02, "reprise : fondu d'entrée");
        assert_eq!(st.mix[100 * 2], 0.5);
    }

    /// Mute / solo pendant la lecture : l'ancien rendu joue jusqu'à la trame de bascule,
    /// puis fondu court vers le nouveau, sans décalage ; si le nouveau n'a pas encore assez
    /// d'avance, l'ancien continue et la bascule se fait plus tard, à la même position.
    #[test]
    fn playback_switch_is_seamless() {
        let (mut st, _p) = monitor_state(2, 0);
        st.monitor = None;
        let (mut old_p, old_c) = rtrb::RingBuffer::new(8192);
        st.cons = Some(old_c);
        // Ancien rendu : la trame n vaut n (repère de position), sur 1000 trames.
        for n in 0..1000 {
            old_p.push(n as f32).unwrap();
            old_p.push(n as f32).unwrap();
        }
        st.process(100);
        assert_eq!(st.shared.out_frames.load(Ordering::Relaxed), 100);
        // Nouveau rendu à partir de la trame 300, pas encore rempli.
        let (mut new_p, new_c) = rtrb::RingBuffer::new(8192);
        st.next = Some((new_c, 300));
        st.process(100);
        st.process(100);
        // Trames 200 à 299 : toujours l'ancien rendu.
        assert_eq!(st.mix[99 * 2], 299.0);
        st.process(100);
        // Bascule impossible (nouveau vide) : l'ancien continue, toujours calé.
        assert_eq!(st.mix[0], 300.0);
        assert!(st.next.is_some());
        // Le nouveau rendu arrive (valeurs négatives : -n à la trame n).
        for n in 300..1000 {
            new_p.push(-(n as f32)).unwrap();
            new_p.push(-(n as f32)).unwrap();
        }
        st.process(100);
        assert!(st.next.is_none(), "bascule faite");
        // Trames 400 à 499 : le retard (300 → 400) a été sauté, fondu puis nouveau rendu.
        assert_eq!(st.mix[99 * 2], -499.0);
        assert!(st.mix[0] > -400.0 && st.mix[0] < 400.0, "fondu enchaîné");
        assert_eq!(st.shared.out_frames.load(Ordering::Relaxed), 500);
        st.process(100);
        assert_eq!(st.mix[0], -500.0);
    }

    /// Une erreur du pilote ne bloque jamais le flux, même si l'interface tient le verrou.
    #[test]
    fn on_error_never_blocks() {
        let sh = Shared::new();
        let guard = sh.last_error.lock();
        sh.on_error(cpal::ErrorKind::DeviceNotAvailable.into(), LOST_PADS);
        drop(guard);
        sh.on_error(cpal::ErrorKind::DeviceNotAvailable.into(), LOST_PADS);
        assert!(
            sh.error_text()
                .is_some_and(|t| t.contains("plus disponible"))
        );
        // Carte disparue : signalée pour la reconnexion automatique (un décrochage, non).
        assert_eq!(sh.device_lost.load(Ordering::Acquire), LOST_PADS);
        let sh2 = Shared::new();
        sh2.on_error(cpal::ErrorKind::Xrun.into(), LOST_INPUT);
        sh2.on_error(cpal::ErrorKind::DeviceChanged.into(), LOST_INPUT);
        // Erreur ambiguë (aussi émise pour une priorité temps réel refusée, flux sain) : pas
        // une perte — un flux vraiment mort est repéré par son battement de cœur.
        sh2.on_error(cpal::ErrorKind::BackendError.into(), LOST_INPUT);
        sh2.on_error(cpal::ErrorKind::RealtimeDenied.into(), LOST_INPUT);
        assert_eq!(sh2.device_lost.load(Ordering::Acquire), 0);
        // Service audio arrêté, carte prise par une autre application : flux mort.
        sh2.on_error(cpal::ErrorKind::HostUnavailable.into(), LOST_OUTPUT);
        sh2.on_error(cpal::ErrorKind::DeviceBusy.into(), LOST_MONITOR);
        assert_eq!(
            sh2.device_lost.load(Ordering::Acquire),
            LOST_OUTPUT | LOST_MONITOR
        );
    }

    #[test]
    fn latency_frames() {
        assert_eq!(to_frames(std::time::Duration::from_millis(10), 48000), 480);
        let sh = Shared::new();
        sh.in_latency.store(100, Ordering::Relaxed);
        sh.out_latency.store(200, Ordering::Relaxed);
        assert_eq!(sh.round_trip_latency(), 300);
    }

    /// Les décrochages du pilote ne comptent que pendant une prise ou une lecture.
    #[test]
    fn xruns_count_only_while_active() {
        let sh = Shared::new();
        sh.on_error(cpal::ErrorKind::Xrun.into(), LOST_INPUT);
        assert_eq!(sh.xruns.load(Ordering::Relaxed), 0, "à l'arrêt : ignoré");
        sh.counting.store(true, Ordering::Relaxed);
        sh.on_error(cpal::ErrorKind::Xrun.into(), LOST_INPUT);
        assert_eq!(sh.xruns.load(Ordering::Relaxed), 1);
        assert!(
            sh.last_error.lock().is_none(),
            "un décrochage n'est pas une erreur"
        );
    }

    /// Stéréo : entrelacement G/D, et la compensation de perte reste en trames entières
    /// (jamais de canaux inversés après une saturation, même avec un tampon impair).
    #[test]
    fn stereo_overrun_keeps_channels() {
        let (prod, mut cons) = rtrb::RingBuffer::new(101);
        let mut t = RecTarget::stereo(2, prod);
        assert_eq!(t.channels, [2, 3]);
        let lr = |_: usize, k: usize| if k == 0 { -1.0 } else { 1.0 };
        assert!(!feed_target(&mut t, 30, lr));
        // 41 places : 20 trames entières seulement, 30 perdues.
        assert!(feed_target(&mut t, 50, lr));
        assert_eq!(t.pending, 30);
        let mut got: Vec<f32> = Vec::new();
        while let Ok(x) = cons.pop() {
            got.push(x);
        }
        assert!(!feed_target(&mut t, 10, lr));
        while let Ok(x) = cons.pop() {
            got.push(x);
        }
        assert_eq!(got.len(), (30 + 50 + 10) * 2);
        let frames: Vec<&[f32; 2]> = got.as_chunks::<2>().0.iter().collect();
        assert!(frames[..50].iter().all(|f| **f == [-1.0, 1.0]));
        assert!(frames[50..80].iter().all(|f| **f == [0.0, 0.0]));
        assert!(frames[80..].iter().all(|f| **f == [-1.0, 1.0]));
    }
}
