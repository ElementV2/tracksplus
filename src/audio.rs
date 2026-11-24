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
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
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
    /// Données perdues car le disque n'a pas suivi.
    pub overruns: AtomicU32,
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
}

impl Shared {
    fn new() -> Self {
        Self {
            meters: (0..MAX_INPUTS).map(|_| AtomicU32::new(0)).collect(),
            rec_frames: AtomicU64::new(0),
            out_frames: AtomicU64::new(0),
            rec_play_offset: AtomicU64::new(0),
            overruns: AtomicU32::new(0),
            underruns: AtomicU32::new(0),
            xruns: AtomicU32::new(0),
            counting: AtomicBool::new(false),
            in_block: AtomicU32::new(0),
            out_block: AtomicU32::new(0),
            last_error: Mutex::new(None),
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
        }
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

    fn on_error(&self, err: cpal::Error) {
        if err.kind() == cpal::ErrorKind::Xrun {
            if self.counting.load(Ordering::Relaxed) {
                self.xruns.fetch_add(1, Ordering::Relaxed);
            }
        } else if let Some(mut e) = self.last_error.try_lock() {
            // Jamais d'attente côté flux audio : message abandonné si l'interface lit.
            *e = Some(err.to_string());
        }
    }
}

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
}

impl RecTarget {
    pub fn new(channel: usize, prod: Producer<f32>) -> Self {
        Self {
            channels: [channel, channel],
            width: 1,
            prod,
            pending: 0,
        }
    }

    /// Paire stéréo `left`, `left + 1`.
    pub fn stereo(left: usize, prod: Producer<f32>) -> Self {
        Self {
            channels: [left, left + 1],
            width: 2,
            prod,
            pending: 0,
        }
    }
}

enum InCmd {
    Record(Vec<RecTarget>),
    Stop,
}

enum OutCmd {
    Play(Consumer<f32>),
    Stop,
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

pub struct Engine {
    pub shared: Arc<Shared>,
    pub status: EngineStatus,
    _input: Option<cpal::Stream>,
    _output: Option<cpal::Stream>,
    /// Second flux de sortie, seulement si le soundboard joue sur un autre périphérique.
    _pad_output: Option<cpal::Stream>,
    /// Flux de sortie dédié au monitoring, s'il est sur un périphérique à part.
    _mon_output: Option<cpal::Stream>,
    in_tx: Option<Sender<InCmd>>,
    out_tx: Option<Sender<OutCmd>>,
    /// Commandes du soundboard (clonable pour le thread MIDI).
    pub pad_tx: Option<Sender<PadCmd>>,
    /// Objets rendus par le callback pour être libérés hors du thread audio.
    garbage_rx: Receiver<Consumer<f32>>,
    sample_garbage_rx: Receiver<Arc<SampleData>>,
    in_garbage_rx: Receiver<Vec<RecTarget>>,
}

impl Engine {
    /// Moteur sans carte son (en cas d'échec d'ouverture).
    pub fn offline() -> Self {
        let (_tx, garbage_rx) = bounded(1);
        let (_tx2, sample_garbage_rx) = bounded(1);
        let (_tx3, in_garbage_rx) = bounded(1);
        Self {
            shared: Arc::new(Shared::new()),
            status: EngineStatus {
                sample_rate: 48000,
                ..Default::default()
            },
            _input: None,
            _output: None,
            _pad_output: None,
            _mon_output: None,
            in_tx: None,
            out_tx: None,
            pad_tx: None,
            garbage_rx,
            sample_garbage_rx,
            in_garbage_rx,
        }
    }

    /// Ouvre la carte. `pad` = sortie du soundboard.
    pub fn open(prefs: &AudioPrefs, pad: &PadOut) -> Result<Self> {
        let host = find_host(prefs.host.as_deref());
        let host_name = host.id().name().to_string();
        let shared = Arc::new(Shared::new());

        let find = |name: &Option<String>, input: bool| -> Option<cpal::Device> {
            match name {
                Some(n) => {
                    let mut devs = if input {
                        host.input_devices().ok()?.collect::<Vec<_>>()
                    } else {
                        host.output_devices().ok()?.collect::<Vec<_>>()
                    };
                    let pos = devs.iter().position(|d| &d.to_string() == n)?;
                    Some(devs.swap_remove(pos))
                }
                None => {
                    if input {
                        host.default_input_device()
                    } else {
                        host.default_output_device()
                    }
                }
            }
        };
        let in_dev = find(&prefs.input, true);
        let out_dev = find(&prefs.output, false);

        // Fréquence : celle demandée, sinon celle par défaut de l'entrée.
        let sample_rate = prefs
            .sample_rate
            .or_else(|| {
                in_dev
                    .as_ref()
                    .and_then(|d| d.default_input_config().ok())
                    .map(|c| c.sample_rate())
            })
            .or_else(|| {
                out_dev
                    .as_ref()
                    .and_then(|d| d.default_output_config().ok())
                    .map(|c| c.sample_rate())
            })
            .unwrap_or(48000);
        let buffer = match prefs.buffer {
            Some(b) => cpal::BufferSize::Fixed(b),
            None => cpal::BufferSize::Default,
        };

        let (garbage_tx, garbage_rx) = bounded(16);
        let (in_garbage_tx, in_garbage_rx) = bounded(16);
        let (sample_garbage_tx, sample_garbage_rx) = bounded(256);
        let mut pad_tx = None;
        let mut status = EngineStatus {
            host: host_name,
            sample_rate,
            buffer: prefs.buffer,
            ..Default::default()
        };

        // Monitoring direct : l'entrée envoie le mix stéréo des canaux écoutés vers la
        // sortie de monitoring par un ring buffer (aucune allocation dans les callbacks).
        let (mon_prod, mon_cons) = rtrb::RingBuffer::<f32>::new(MON_RING * 2);
        let mut mon_prod = Some(mon_prod);
        let mut mon_cons = Some(mon_cons);

        let mut input = None;
        let mut in_tx = None;
        if let Some(dev) = &in_dev {
            let (fmt, channels) = pick_config(dev, sample_rate, true)?;
            let cfg = StreamConfig {
                channels,
                sample_rate,
                buffer_size: buffer,
            };
            let (tx, rx) = bounded(8);
            let st = InState {
                channels: channels as usize,
                rx,
                targets: Vec::with_capacity(MAX_INPUTS),
                garbage: in_garbage_tx,
                shared: shared.clone(),
                monitor: mon_prod.take(),
                sample_rate,
            };
            let stream = match fmt {
                SampleFormat::F32 => build_input::<f32>(dev, cfg, st),
                SampleFormat::I32 => build_input::<i32>(dev, cfg, st),
                SampleFormat::I24 => build_input::<cpal::I24>(dev, cfg, st),
                SampleFormat::I16 => build_input::<i16>(dev, cfg, st),
                f => bail!("Format d'entrée non géré : {f:?}"),
            }
            .with_context(|| format!("Ouverture de l'entrée « {dev} »"))?;
            stream.play().context("Démarrage du flux d'entrée")?;
            status.input = Some(dev.to_string());
            status.in_channels = channels;
            status.in_format = Some(fmt);
            input = Some(stream);
            in_tx = Some(tx);
        }

        // Soundboard sur un autre périphérique que l'écoute : second flux de sortie.
        let out_name = out_dev.as_ref().map(|d| d.to_string());
        let pad_dev = match pad
            .device
            .as_ref()
            .filter(|n| Some(*n) != out_name.as_ref())
        {
            Some(n) => {
                let d = host
                    .output_devices()
                    .ok()
                    .and_then(|mut it| it.find(|d| &d.to_string() == n));
                if d.is_none() {
                    *shared.last_error.lock() = Some(format!(
                        "Sortie soundboard « {n} » introuvable : pads sur l'écoute"
                    ));
                }
                d
            }
            None => None,
        };
        // Monitoring : sur le flux d'écoute, sur celui du soundboard, ou sur un flux à part.
        let pad_name = pad_dev.as_ref().map(|d| d.to_string());
        let mon_name = prefs.monitor_output.clone().or_else(|| out_name.clone());
        let mon_on_main = mon_name == out_name;
        let mon_on_pads = !mon_on_main && pad_name.is_some() && mon_name == pad_name;
        let mon_dev = match (&mon_name, mon_on_main || mon_on_pads) {
            (Some(n), false) => {
                let d = host
                    .output_devices()
                    .ok()
                    .and_then(|mut it| it.find(|d| &d.to_string() == n));
                if d.is_none() {
                    *shared.last_error.lock() =
                        Some(format!("Sortie monitoring « {n} » introuvable"));
                }
                d
            }
            _ => None,
        };
        let mon_left = prefs.monitor_pair() as usize;
        let main_mon = if mon_on_main { mon_cons.take() } else { None };
        let pads_mon = if mon_on_pads { mon_cons.take() } else { None };
        let main_has_mon = main_mon.is_some();
        let pads_has_mon = pads_mon.is_some();

        // Les commandes des pads vont au flux qui porte le soundboard ; l'autre reçoit un
        // canal vide (son lecteur de pads reste muet).
        let (ptx, prx) = bounded(256);
        let idle_pads = || bounded::<PadCmd>(1).1;
        let (main_pads, sep_pads) = if pad_dev.is_some() {
            (idle_pads(), Some(prx))
        } else {
            (prx, None)
        };

        let mut output = None;
        let mut out_tx = None;
        let mut pads_ok = false;
        if let Some(dev) = &out_dev {
            let (tx, rx) = bounded(8);
            let sgt = sample_garbage_tx.clone();
            let gt = garbage_tx.clone();
            let sh = shared.clone();
            let res = start_output(dev, sample_rate, buffer, move |channels| {
                let last = channels.saturating_sub(1) as usize;
                OutState::new(
                    channels as usize,
                    (prefs.out_left as usize).min(last),
                    rx,
                    gt,
                    sh,
                    Sampler::new(main_pads, sgt, (pad.left as usize).min(last), sample_rate),
                )
                .with_monitor(main_mon, mon_left)
                .main_output(sample_rate)
            });
            match res {
                Ok((s, channels)) => {
                    status.output = Some(dev.to_string());
                    status.out_channels = channels;
                    status.pad_out_channels = channels;
                    output = Some(s);
                    out_tx = Some(tx);
                    pads_ok = sep_pads.is_none();
                    status.monitor_ok |= main_has_mon;
                }
                Err(e) => *shared.last_error.lock() = Some(format!("Sortie : {e:#}")),
            }
        }
        let mut pad_output = None;
        if let (Some(dev), Some(prx)) = (&pad_dev, sep_pads) {
            let sh = shared.clone();
            let gt = garbage_tx.clone();
            let sgt = sample_garbage_tx.clone();
            let res = start_output(dev, sample_rate, buffer, move |channels| {
                let last = channels.saturating_sub(1) as usize;
                // Pas de lecture multipiste sur ce flux : canal de commandes vide.
                OutState::new(
                    channels as usize,
                    0,
                    bounded::<OutCmd>(1).1,
                    gt,
                    sh,
                    Sampler::new(prx, sgt, (pad.left as usize).min(last), sample_rate),
                )
                .with_monitor(pads_mon, mon_left)
            });
            match res {
                Ok((s, channels)) => {
                    status.pad_output = Some(dev.to_string());
                    status.pad_out_channels = channels;
                    pad_output = Some(s);
                    pads_ok = true;
                    status.monitor_ok |= pads_has_mon;
                }
                Err(e) => *shared.last_error.lock() = Some(format!("Sortie soundboard : {e:#}")),
            }
        }
        if pads_ok {
            pad_tx = Some(ptx);
        }
        let mut mon_output = None;
        if let (Some(dev), Some(cons)) = (&mon_dev, mon_cons.take()) {
            let sh = shared.clone();
            let gt = garbage_tx.clone();
            let res = start_output(dev, sample_rate, buffer, move |channels| {
                // Flux sans lecture ni pads : canaux de commandes vides.
                OutState::new(
                    channels as usize,
                    0,
                    bounded::<OutCmd>(1).1,
                    gt,
                    sh,
                    Sampler::new(bounded::<PadCmd>(1).1, sample_garbage_tx, 0, sample_rate),
                )
                .with_monitor(Some(cons), mon_left)
            });
            match res {
                Ok((s, _)) => {
                    status.monitor_output = Some(dev.to_string());
                    status.monitor_ok = true;
                    mon_output = Some(s);
                }
                Err(e) => *shared.last_error.lock() = Some(format!("Sortie monitoring : {e:#}")),
            }
        }

        Ok(Self {
            shared,
            status,
            _input: input,
            _output: output,
            _pad_output: pad_output,
            _mon_output: mon_output,
            in_tx,
            out_tx,
            pad_tx,
            garbage_rx,
            sample_garbage_rx,
            in_garbage_rx,
        })
    }

    pub fn has_input(&self) -> bool {
        self.in_tx.is_some()
    }

    pub fn has_output(&self) -> bool {
        self.out_tx.is_some()
    }

    pub fn start_record(&self, targets: Vec<RecTarget>) -> Result<()> {
        let tx = self
            .in_tx
            .as_ref()
            .ok_or_else(|| anyhow!("Aucune entrée audio"))?;
        self.shared.rec_frames.store(0, Ordering::Relaxed);
        // Compteurs propres à cette prise.
        for c in [
            &self.shared.xruns,
            &self.shared.underruns,
            &self.shared.overruns,
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
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match self.in_garbage_rx.recv_timeout(left) {
                // Liste vide = celle remplacée au démarrage ; on attend celle de la prise.
                Ok(v) if v.is_empty() => continue,
                Ok(_) => return true,
                Err(_) => return false,
            }
        }
    }

    pub fn start_playback(&self, cons: Consumer<f32>) {
        // Remis à zéro ici et dans le callback : pas de saut de la tête au démarrage.
        self.shared.out_frames.store(0, Ordering::Relaxed);
        self.shared.counting.store(true, Ordering::Relaxed);
        if let Some(tx) = &self.out_tx {
            let _ = tx.try_send(OutCmd::Play(cons));
        }
    }

    pub fn stop_playback(&self) {
        if let Some(tx) = &self.out_tx {
            let _ = tx.try_send(OutCmd::Stop);
        }
    }

    /// À appeler régulièrement depuis l'interface.
    pub fn collect_garbage(&self) {
        while self.garbage_rx.try_recv().is_ok() {}
        while self.sample_garbage_rx.try_recv().is_ok() {}
        while self.in_garbage_rx.try_recv().is_ok() {}
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
        .ok_or_else(|| anyhow!("« {dev} » ne supporte pas {} Hz", sr))
}

struct InState {
    channels: usize,
    rx: Receiver<InCmd>,
    targets: Vec<RecTarget>,
    /// Les listes de cibles remplacées sont rendues à l'interface pour y être libérées.
    garbage: Sender<Vec<RecTarget>>,
    shared: Arc<Shared>,
    /// Vers la sortie de monitoring (mix stéréo entrelacé des canaux écoutés).
    monitor: Option<Producer<f32>>,
    sample_rate: u32,
}

impl InState {
    fn dispose(&self, v: Vec<RecTarget>) {
        if let Err(e) = self.garbage.try_send(v) {
            std::mem::forget(e.into_inner());
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
            // Latence capture → callback (compensation des prises en re-recording).
            let ts = info.timestamp();
            st.shared.in_latency.store(
                to_frames(ts.callback.duration_since(ts.capture), st.sample_rate),
                Ordering::Relaxed,
            );
            while let Ok(cmd) = st.rx.try_recv() {
                // Aucune libération mémoire ici : l'ancienne liste part vers l'interface,
                // qui relâche les producteurs (le thread disque voit alors la fin de prise).
                match cmd {
                    InCmd::Record(t) => {
                        st.shared.rec_play_offset.store(
                            st.shared.out_frames.load(Ordering::Relaxed),
                            Ordering::Relaxed,
                        );
                        let old = std::mem::replace(&mut st.targets, t);
                        st.dispose(old);
                    }
                    InCmd::Stop => {
                        let old = std::mem::take(&mut st.targets);
                        st.dispose(old);
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

            for c in 0..ch.min(MAX_INPUTS) {
                let mut peak = 0f32;
                for f in 0..frames {
                    peak = peak.max(data[f * ch + c].to_sample::<f32>().abs());
                }
                sh.meters[c].fetch_max(peak.to_bits(), Ordering::Relaxed);
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
                    chunk.fill_from_iter((0..n).flat_map(|f| {
                        let (mut l, mut r) = (0f32, 0f32);
                        for &(c, m) in &act[..na] {
                            let s = data[f * ch + c].to_sample::<f32>();
                            match m {
                                2 => l += s,
                                3 => r += s,
                                _ => {
                                    l += s;
                                    r += s;
                                }
                            }
                        }
                        [l, r]
                    }));
                }
            }

            if !st.targets.is_empty() {
                for t in &mut st.targets {
                    let cs = t.channels;
                    let lost = feed_target(t, frames, |f, k| {
                        let c = cs[k];
                        if c < ch {
                            data[f * ch + c].to_sample::<f32>()
                        } else {
                            0.0
                        }
                    });
                    if lost {
                        sh.overruns.fetch_add(1, Ordering::Relaxed);
                    }
                }
                sh.rec_frames.fetch_add(frames as u64, Ordering::Relaxed);
            }
        },
        move |e| err_shared.on_error(e),
        None,
    )?;
    Ok(stream)
}

/// Écrit un bloc d'une entrée dans son tampon disque, sans jamais décaler la piste :
/// si des trames ont dû être perdues (disque en retard), autant de silence est inséré
/// à leur place dès que le tampon a de nouveau de la place. Retourne `true` en cas de perte.
/// `sample(trame, k)` donne l'échantillon du k-ième canal de la cible. Tout se compte en
/// trames entières : une piste stéréo ne se désentrelace jamais.
fn feed_target(t: &mut RecTarget, frames: usize, sample: impl Fn(usize, usize) -> f32) -> bool {
    let w = t.width.max(1);
    if t.pending > 0 {
        let z = t.pending.min(t.prod.slots() / w);
        if let Ok(chunk) = t.prod.write_chunk_uninit(z * w) {
            chunk.fill_from_iter(std::iter::repeat_n(0.0f32, z * w));
        }
        t.pending -= z;
    }
    let n = if t.pending > 0 {
        0
    } else {
        frames.min(t.prod.slots() / w)
    };
    if n > 0
        && let Ok(chunk) = t.prod.write_chunk_uninit(n * w)
    {
        chunk.fill_from_iter((0..n * w).map(|i| sample(i / w, i % w)));
    }
    if n < frames {
        t.pending += frames - n;
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
    garbage: Sender<Consumer<f32>>,
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
        garbage: Sender<Consumer<f32>>,
        shared: Arc<Shared>,
        sampler: Sampler,
    ) -> Self {
        Self {
            channels,
            left,
            rx,
            cons: None,
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
        }
    }

    /// Flux d'écoute principal (mesure de la latence de sortie à `sample_rate`).
    fn main_output(mut self, sample_rate: u32) -> Self {
        self.main = true;
        self.sample_rate = sample_rate;
        self
    }

    /// Ce flux joue aussi le monitoring, sur la paire `left`, `left + 1`.
    fn with_monitor(mut self, cons: Option<Consumer<f32>>, left: usize) -> Self {
        let last = self.channels.saturating_sub(1);
        self.monitor = cons.map(|cons| MonitorOut::new(cons, left.min(last)));
        self
    }

    fn release(&mut self) {
        if let Some(c) = self.cons.take() {
            // Le ring buffer est libéré par l'interface, jamais par le thread audio.
            if let Err(e) = self.garbage.try_send(c) {
                std::mem::forget(e.into_inner());
            }
        }
    }

    /// Calcule `frames` trames (≤ MAX_BLOCK) dans `self.mix`.
    fn process(&mut self, frames: usize) {
        let ch = self.channels;
        let sh = &*self.shared;
        let play = &mut self.play[..frames * 2];
        play.fill(0.0);
        if let Some(cons) = self.cons.as_mut() {
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

fn build_output<T>(dev: &cpal::Device, cfg: StreamConfig, mut st: OutState) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32> + Send + 'static,
{
    let err_shared = st.shared.clone();
    let stream = dev.build_output_stream::<T, _, _>(
        cfg,
        move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
            if st.main {
                // Latence callback → restitution (compensation des prises en re-recording).
                let ts = info.timestamp();
                st.shared.out_latency.store(
                    to_frames(ts.playback.duration_since(ts.callback), st.sample_rate),
                    Ordering::Relaxed,
                );
            }
            while let Ok(cmd) = st.rx.try_recv() {
                match cmd {
                    OutCmd::Play(c) => {
                        st.release();
                        st.cons = Some(c);
                        st.play_fade_in = false;
                        // Remis à zéro ici, au moment exact où la nouvelle lecture commence.
                        st.shared.out_frames.store(0, Ordering::Relaxed);
                    }
                    OutCmd::Stop => st.release(),
                }
            }
            let ch = st.channels;
            if ch == 0 {
                return;
            }
            let total = data.len() / ch;
            st.shared.out_block.store(total as u32, Ordering::Relaxed);
            let mut done = 0;
            while done < total {
                let n = (total - done).min(MAX_BLOCK);
                st.process(n);
                let out = &mut data[done * ch..(done + n) * ch];
                for (d, &s) in out.iter_mut().zip(&st.mix[..n * ch]) {
                    // Jamais de NaN / infini vers la carte (sécurité des enceintes).
                    *d = T::from_sample(if s.is_finite() { s } else { 0.0 });
                }
                done += n;
            }
        },
        move |e| err_shared.on_error(e),
        None,
    )?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
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
            bounded::<OutCmd>(1).1,
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

    /// Une erreur du pilote ne bloque jamais le flux, même si l'interface tient le verrou.
    #[test]
    fn on_error_never_blocks() {
        let sh = Shared::new();
        let guard = sh.last_error.lock();
        sh.on_error(cpal::ErrorKind::DeviceNotAvailable.into());
        drop(guard);
        sh.on_error(cpal::ErrorKind::DeviceNotAvailable.into());
        assert!(sh.last_error.lock().is_some());
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
        sh.on_error(cpal::ErrorKind::Xrun.into());
        assert_eq!(sh.xruns.load(Ordering::Relaxed), 0, "à l'arrêt : ignoré");
        sh.counting.store(true, Ordering::Relaxed);
        sh.on_error(cpal::ErrorKind::Xrun.into());
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
