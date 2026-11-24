//! Moteur audio : choix de la carte son, flux d'entrée (enregistrement + vumètres)
//! et flux de sortie (lecture).
//!
//! Les callbacks audio ne font aucune allocation ni aucun verrou : ils échangent les
//! données avec les threads disque via des ring buffers `rtrb`.

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
        }
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

/// Une entrée à enregistrer : canal de la carte → ring buffer vers le disque.
pub struct RecTarget {
    pub channel: usize,
    pub prod: Producer<f32>,
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
    /// Objets rendus par le callback pour être libérés hors du thread audio.
    garbage_rx: Receiver<Consumer<f32>>,
}

impl Engine {
    /// Moteur sans carte son (en cas d'échec d'ouverture).
    pub fn offline() -> Self {
        let (_tx, garbage_rx) = bounded(1);
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
            garbage_rx,
        }
    }

    pub fn open(prefs: &AudioPrefs) -> Result<Self> {
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
                    let left = (prefs.out_left as usize).min(channels.saturating_sub(1) as usize);
                    let st = OutState {
                        channels: channels as usize,
                        left,
                        right: (left + 1).min(channels as usize - 1),
                        rx,
                        cons: None,
                        garbage: garbage_tx,
                        shared: shared.clone(),
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
            garbage_rx,
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

    pub fn stop_record(&self) {
        if let Some(tx) = &self.in_tx {
            let _ = tx.try_send(InCmd::Stop);
        }
    }

    pub fn start_playback(&self, cons: Consumer<f32>) {
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
    shared: Arc<Shared>,
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
                match cmd {
                    InCmd::Record(t) => st.targets = t,
                    // Les producteurs sont relâchés ici : le thread disque voit alors la fin.
                    InCmd::Stop => st.targets.clear(),
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
                    let c = t.channel;
                    let n = frames.min(t.prod.slots());
                    if n < frames {
                        sh.overruns.fetch_add(1, Ordering::Relaxed);
                    }
                    if let Ok(chunk) = t.prod.write_chunk_uninit(n) {
                        let it = (0..n).map(|f| {
                            if c < ch {
                                data[f * ch + c].to_sample::<f32>()
                            } else {
                                0.0
                            }
                        });
                        chunk.fill_from_iter(it);
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

struct OutState {
    channels: usize,
    left: usize,
    right: usize,
    rx: Receiver<OutCmd>,
    cons: Option<Consumer<f32>>,
    garbage: Sender<Consumer<f32>>,
    shared: Arc<Shared>,
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
                    }
                    OutCmd::Stop => st.release(),
                }
            }
            for s in data.iter_mut() {
                *s = T::EQUILIBRIUM;
            }
            let ch = st.channels;
            let Some(cons) = st.cons.as_mut() else {
                return;
            };
            if ch == 0 {
                return;
            }
            let frames = data.len() / ch;
            let avail = cons.slots() / 2;
            let n = frames.min(avail);
            if n < frames && !cons.is_abandoned() {
                st.shared.underruns.fetch_add(1, Ordering::Relaxed);
            }
            if let Ok(chunk) = cons.read_chunk(n * 2) {
                let (a, b) = chunk.as_slices();
                let mut it = a.iter().chain(b.iter());
                for f in 0..n {
                    let l = *it.next().unwrap_or(&0.0);
                    let r = *it.next().unwrap_or(&0.0);
                    if st.left == st.right {
                        data[f * ch + st.left] = T::from_sample(0.5 * (l + r));
                    } else {
                        data[f * ch + st.left] = T::from_sample(l);
                        data[f * ch + st.right] = T::from_sample(r);
                    }
                }
                chunk.commit_all();
            }
            st.shared.out_frames.fetch_add(n as u64, Ordering::Relaxed);
        },
        move |e| err_shared.on_error(e),
        None,
    )?;
    Ok(stream)
}
