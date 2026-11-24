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
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

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
}

impl AudioPrefs {
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
                let _ = std::fs::write(p, s);
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub name: String,
    pub channels: u16,
    pub rates: Vec<u32>,
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

pub fn default_host_name() -> String {
    cpal::default_host().id().name().to_string()
}

fn ranges_info(
    name: String,
    ranges: impl Iterator<Item = cpal::SupportedStreamConfigRange>,
) -> DeviceInfo {
    let mut channels = 0;
    let mut rates = Vec::new();
    for r in ranges {
        channels = channels.max(r.channels());
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
                let info = ranges_info(name.clone(), cfgs);
                if info.channels > 0 {
                    ins.push(info);
                }
            }
            if let Ok(cfgs) = d.supported_output_configs() {
                let info = ranges_info(name, cfgs);
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
    /// Données perdues car le disque n'a pas suivi.
    pub overruns: AtomicU32,
    /// Trous de lecture.
    pub underruns: AtomicU32,
    /// Xruns signalés par le pilote.
    pub xruns: AtomicU32,
    pub last_error: Mutex<Option<String>>,
    /// Crêtes L/R de la lecture multipiste (bits f32).
    pub master_play: [AtomicU32; 2],
    /// Crêtes L/R du soundboard (bits f32).
    pub master_pad: [AtomicU32; 2],
    /// Position de lecture de chaque pad (+1, 0 = arrêté).
    pub pad_pos: Vec<AtomicU64>,
}

impl Shared {
    fn new() -> Self {
        Self {
            meters: (0..MAX_INPUTS).map(|_| AtomicU32::new(0)).collect(),
            rec_frames: AtomicU64::new(0),
            out_frames: AtomicU64::new(0),
            overruns: AtomicU32::new(0),
            underruns: AtomicU32::new(0),
            xruns: AtomicU32::new(0),
            last_error: Mutex::new(None),
            master_play: [AtomicU32::new(0), AtomicU32::new(0)],
            master_pad: [AtomicU32::new(0), AtomicU32::new(0)],
            pad_pos: (0..MAX_PADS).map(|_| AtomicU64::new(0)).collect(),
        }
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
            self.xruns.fetch_add(1, Ordering::Relaxed);
        } else {
            *self.last_error.lock() = Some(err.to_string());
        }
    }
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
    pub buffer: Option<u32>,
    pub in_format: Option<SampleFormat>,
}

pub struct Engine {
    pub shared: Arc<Shared>,
    pub status: EngineStatus,
    _input: Option<cpal::Stream>,
    _output: Option<cpal::Stream>,
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
            in_tx: None,
            out_tx: None,
            pad_tx: None,
            garbage_rx,
            sample_garbage_rx,
            in_garbage_rx,
        }
    }

    /// Ouvre la carte. `pad_left` = premier canal de sortie du soundboard.
    pub fn open(prefs: &AudioPrefs, pad_left: u16) -> Result<Self> {
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
        if in_dev.is_none() && out_dev.is_none() {
            bail!("Aucune carte son trouvée sur le pilote {host_name}");
        }
        if let (Some(name), None) = (&prefs.input, &in_dev) {
            bail!("Entrée « {name} » introuvable (carte débranchée ?)");
        }

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

        let mut output = None;
        let mut out_tx = None;
        if let Some(dev) = &out_dev {
            match pick_config(dev, sample_rate, false) {
                Ok((fmt, channels)) => {
                    let cfg = StreamConfig {
                        channels,
                        sample_rate,
                        buffer_size: buffer,
                    };
                    let (tx, rx) = bounded(8);
                    let (ptx, prx) = bounded(256);
                    let last = channels.saturating_sub(1) as usize;
                    let left = (prefs.out_left as usize).min(last);
                    let st = OutState {
                        channels: channels as usize,
                        left,
                        rx,
                        cons: None,
                        garbage: garbage_tx,
                        shared: shared.clone(),
                        sampler: Sampler::new(
                            prx,
                            sample_garbage_tx,
                            (pad_left as usize).min(last),
                            sample_rate,
                        ),
                        mix: vec![0.0; MAX_BLOCK * channels as usize],
                        play: vec![0.0; MAX_BLOCK * 2],
                        pads: vec![0.0; MAX_BLOCK * 2],
                    };
                    let res = match fmt {
                        SampleFormat::F32 => build_output::<f32>(dev, cfg, st),
                        SampleFormat::I32 => build_output::<i32>(dev, cfg, st),
                        SampleFormat::I24 => build_output::<cpal::I24>(dev, cfg, st),
                        SampleFormat::I16 => build_output::<i16>(dev, cfg, st),
                        f => Err(anyhow!("Format de sortie non géré : {f:?}")),
                    };
                    match res.and_then(|s| {
                        s.play().context("Démarrage du flux de sortie")?;
                        Ok(s)
                    }) {
                        Ok(s) => {
                            status.output = Some(dev.to_string());
                            status.out_channels = channels;
                            output = Some(s);
                            out_tx = Some(tx);
                            pad_tx = Some(ptx);
                        }
                        Err(e) => *shared.last_error.lock() = Some(format!("Sortie : {e:#}")),
                    }
                }
                Err(e) => *shared.last_error.lock() = Some(format!("Sortie : {e:#}")),
            }
        }

        Ok(Self {
            shared,
            status,
            _input: input,
            _output: output,
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
        move |data: &[T], _| {
            while let Ok(cmd) = st.rx.try_recv() {
                // Aucune libération mémoire ici : l'ancienne liste part vers l'interface,
                // qui relâche les producteurs (le thread disque voit alors la fin de prise).
                match cmd {
                    InCmd::Record(t) => {
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

            for c in 0..ch.min(MAX_INPUTS) {
                let mut peak = 0f32;
                for f in 0..frames {
                    peak = peak.max(data[f * ch + c].to_sample::<f32>().abs());
                }
                sh.meters[c].fetch_max(peak.to_bits(), Ordering::Relaxed);
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
}

impl OutState {
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
    }
}

fn build_output<T>(dev: &cpal::Device, cfg: StreamConfig, mut st: OutState) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32> + Send + 'static,
{
    let err_shared = st.shared.clone();
    let stream = dev.build_output_stream::<T, _, _>(
        cfg,
        move |data: &mut [T], _| {
            while let Ok(cmd) = st.rx.try_recv() {
                match cmd {
                    OutCmd::Play(c) => {
                        st.release();
                        st.cons = Some(c);
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
            let mut done = 0;
            while done < total {
                let n = (total - done).min(MAX_BLOCK);
                st.process(n);
                let out = &mut data[done * ch..(done + n) * ch];
                for (d, s) in out.iter_mut().zip(&st.mix[..n * ch]) {
                    *d = T::from_sample(*s);
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
