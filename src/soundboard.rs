//! Modèle du soundboard (pages de pads), sauvegarde, et chargement des sons
//! (WAV, AIFF, MP3, AAC/M4A, FLAC, OGG…) rééchantillonnés à la fréquence de la carte.

use crate::sampler::{MAX_PAGES, PADS_PER_PAGE, PadMode, PadParams, SAMPLE_PEAK, SampleData};
use anyhow::{Context, Result, anyhow};
use crossbeam_channel::{Receiver, Sender, unbounded};
use rubato::{FftFixedIn, Resampler};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Pad {
    pub name: String,
    pub path: PathBuf,
    #[serde(default)]
    pub color: u8,
    #[serde(default)]
    pub gain_db: f32,
    /// Début (s).
    #[serde(default)]
    pub start: f64,
    /// Fin (s). `None` = fin du fichier.
    #[serde(default)]
    pub end: Option<f64>,
    #[serde(default)]
    pub fade_in: f64,
    #[serde(default)]
    pub fade_out: f64,
    #[serde(default)]
    pub mode: PadMode,
    #[serde(default)]
    pub looped: bool,
    /// Nom de touche clavier (egui::Key::name).
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub midi: Option<MidiKey>,
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
            mode: PadMode::Toggle,
            looped: false,
            key: None,
            midi: None,
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
    if db <= -60.0 {
        0.0
    } else {
        10f32.powf(db / 20.0)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Page {
    pub name: String,
    pub pads: Vec<Option<Pad>>,
}

impl Page {
    pub fn new(name: String) -> Self {
        Self {
            name,
            pads: vec![None; PADS_PER_PAGE],
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Board {
    pub cols: usize,
    pub rows: usize,
    pub pages: Vec<Page>,
    /// Premier canal de sortie du soundboard (0-based).
    #[serde(default)]
    pub out_left: u16,
    #[serde(default)]
    pub master_db: f32,
    /// Durée du fondu de « Stop tout » (s).
    #[serde(default = "default_panic_fade")]
    pub panic_fade: f64,
    #[serde(default)]
    pub midi_port: Option<String>,
    /// Mode des nouveaux pads.
    #[serde(default)]
    pub default_mode: PadMode,
}

fn default_panic_fade() -> f64 {
    1.0
}

impl Default for Board {
    fn default() -> Self {
        Self {
            cols: 6,
            rows: 4,
            pages: vec![Page::new("Page 1".into())],
            out_left: 0,
            master_db: 0.0,
            panic_fade: 1.0,
            midi_port: None,
            default_mode: PadMode::Toggle,
        }
    }
}

impl Board {
    pub fn default_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("TracksPlus").join("soundboard.json"))
    }

    pub fn load(path: &Path) -> Result<Self> {
        let s = std::fs::read_to_string(path)
            .with_context(|| format!("Lecture de {}", path.display()))?;
        let mut b: Board = serde_json::from_str(&s).context("Fichier de soundboard invalide")?;
        b.normalize();
        Ok(b)
    }

    pub fn load_default() -> Self {
        Self::default_path()
            .and_then(|p| Self::load(&p).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(tmp, path)?;
        Ok(())
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

/// Cache des sons chargés, partagé par tous les pads.
pub struct SampleCache {
    pub map: HashMap<PathBuf, LoadState>,
    sample_rate: u32,
    tx: Sender<(PathBuf, u32, Result<SampleData, String>)>,
    rx: Receiver<(PathBuf, u32, Result<SampleData, String>)>,
}

impl SampleCache {
    pub fn new(sample_rate: u32) -> Self {
        let (tx, rx) = unbounded();
        Self {
            map: HashMap::new(),
            sample_rate,
            tx,
            rx,
        }
    }

    pub fn set_sample_rate(&mut self, sr: u32) {
        if sr != self.sample_rate {
            self.sample_rate = sr;
            let paths: Vec<_> = self.map.keys().cloned().collect();
            self.map.clear();
            for p in paths {
                self.request(&p, None);
            }
        }
    }

    pub fn request(&mut self, path: &Path, ctx: Option<&eframe::egui::Context>) {
        if self.map.contains_key(path) {
            return;
        }
        self.map.insert(path.to_path_buf(), LoadState::Loading);
        let tx = self.tx.clone();
        let p = path.to_path_buf();
        let sr = self.sample_rate;
        let ctx = ctx.cloned();
        std::thread::spawn(move || {
            let r = load_sample(&p, sr).map_err(|e| format!("{e:#}"));
            let _ = tx.send((p, sr, r));
            if let Some(c) = ctx {
                c.request_repaint();
            }
        });
    }

    pub fn reload(&mut self, path: &Path) {
        self.map.remove(path);
        self.request(path, None);
    }

    pub fn poll(&mut self) -> bool {
        let mut any = false;
        while let Ok((p, sr, r)) = self.rx.try_recv() {
            if sr != self.sample_rate {
                continue;
            }
            any = true;
            self.map.insert(
                p,
                match r {
                    Ok(s) => LoadState::Ready(Arc::new(s)),
                    Err(e) => LoadState::Failed(e),
                },
            );
        }
        any
    }

    pub fn get(&self, path: &Path) -> Option<&Arc<SampleData>> {
        match self.map.get(path) {
            Some(LoadState::Ready(s)) => Some(s),
            _ => None,
        }
    }
}

/// Décode un fichier audio en stéréo f32 à la fréquence `target_sr`.
pub fn load_sample(path: &Path, target_sr: u32) -> Result<SampleData> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
    use symphonia::core::errors::Error as SymError;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let file =
        std::fs::File::open(path).with_context(|| format!("Ouverture de {}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| anyhow!("Format non reconnu : {e}"))?;
    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| anyhow!("Aucune piste audio"))?;
    let track_id = track.id;
    let src_sr = track.codec_params.sample_rate.unwrap_or(target_sr);
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| anyhow!("Codec non supporté : {e}"))?;

    let mut left = Vec::new();
    let mut right = Vec::new();
    let mut buf: Option<SampleBuffer<f32>> = None;
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(SymError::ResetRequired) => break,
            Err(e) => return Err(anyhow!("Lecture : {e}")),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(SymError::DecodeError(_)) => continue,
            Err(e) => return Err(anyhow!("Décodage : {e}")),
        };
        let spec = *decoded.spec();
        let ch = spec.channels.count().max(1);
        let b = buf.get_or_insert_with(|| SampleBuffer::new(decoded.capacity() as u64, spec));
        if b.capacity() < decoded.capacity() * ch {
            *b = SampleBuffer::new(decoded.capacity() as u64, spec);
        }
        b.copy_interleaved_ref(decoded);
        for frame in b.samples().chunks(ch) {
            left.push(frame[0]);
            right.push(if ch > 1 { frame[1] } else { frame[0] });
        }
    }
    if left.is_empty() {
        return Err(anyhow!("Fichier vide"));
    }

    if src_sr != target_sr {
        let (l, r) = resample_stereo(&left, &right, src_sr, target_sr)?;
        left = l;
        right = r;
    }
    let frames = left.len();
    let mut data = Vec::with_capacity(frames * 2);
    let mut peaks = Vec::with_capacity(frames / SAMPLE_PEAK + 1);
    for (i, (l, r)) in left.iter().zip(&right).enumerate() {
        data.push(*l);
        data.push(*r);
        let m = 0.5 * (l + r);
        if i % SAMPLE_PEAK == 0 {
            peaks.push([m, m]);
        }
        let p = peaks.last_mut().unwrap();
        p[0] = p[0].min(l.min(*r));
        p[1] = p[1].max(l.max(*r));
    }
    Ok(SampleData {
        sample_rate: target_sr,
        frames,
        data,
        peaks,
    })
}

fn resample_stereo(l: &[f32], r: &[f32], from: u32, to: u32) -> Result<(Vec<f32>, Vec<f32>)> {
    const CHUNK: usize = 4096;
    let mut rs = FftFixedIn::<f32>::new(from as usize, to as usize, CHUNK, 2, 2)?;
    let expected = (l.len() as u64 * to as u64).div_ceil(from as u64) as usize;
    let mut skip = rs.output_delay();
    let mut ol = Vec::with_capacity(expected + CHUNK);
    let mut or = Vec::with_capacity(expected + CHUNK);
    fn push(b: Vec<Vec<f32>>, skip: &mut usize, ol: &mut Vec<f32>, or: &mut Vec<f32>) {
        let n = b[0].len();
        let s = (*skip).min(n);
        *skip -= s;
        ol.extend_from_slice(&b[0][s..]);
        or.extend_from_slice(&b[1][s..]);
    }
    let mut pos = 0;
    while pos < l.len() {
        let n = (l.len() - pos).min(CHUNK);
        let input = [&l[pos..pos + n], &r[pos..pos + n]];
        let out = if n == CHUNK {
            rs.process(&input, None)?
        } else {
            rs.process_partial(Some(&input), None)?
        };
        push(out, &mut skip, &mut ol, &mut or);
        pos += n;
    }
    let mut guard = 0;
    while ol.len() < expected && guard < 32 {
        push(
            rs.process_partial::<&[f32]>(None, None)?,
            &mut skip,
            &mut ol,
            &mut or,
        );
        guard += 1;
    }
    ol.truncate(expected);
    or.truncate(expected);
    Ok((ol, or))
}

pub fn is_audio_file(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| AUDIO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
