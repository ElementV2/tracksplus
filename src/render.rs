//! Lecture des prises depuis le disque (lecture / export) et calcul des formes d'onde.

use crate::project::{Clip, Project};
use hound::{SampleFormat, WavReader};
use parking_lot::RwLock;
use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;
use std::sync::Arc;

/// Nombre d'échantillons résumés par un point de forme d'onde.
pub const PEAK_BLOCK: usize = 512;

/// Facteur entre les deux niveaux de détail de la forme d'onde.
pub const PEAK_L1: usize = 16;

/// Forme d'onde : (min, max) quantifiés sur i16, sur deux niveaux de détail
/// (`PEAK_BLOCK` et `PEAK_BLOCK * PEAK_L1` échantillons par point).
#[derive(Default)]
pub struct PeakData {
    pub l0: Vec<[i16; 2]>,
    pub l1: Vec<[i16; 2]>,
}

impl PeakData {
    pub fn extend(&mut self, new: &[[i16; 2]]) {
        for &p in new {
            self.l0.push(p);
            let i = self.l0.len() - 1;
            if i.is_multiple_of(PEAK_L1) {
                self.l1.push(p);
            } else if let Some(last) = self.l1.last_mut() {
                last[0] = last[0].min(p[0]);
                last[1] = last[1].max(p[1]);
            }
        }
    }
}

pub type Peaks = Arc<RwLock<PeakData>>;

pub fn to_peak(min: f32, max: f32) -> [i16; 2] {
    [
        (min.clamp(-1.0, 1.0) * 32767.0) as i16,
        (max.clamp(-1.0, 1.0) * 32767.0) as i16,
    ]
}

/// Accumulateur de forme d'onde utilisé pendant l'enregistrement.
#[derive(Default)]
pub struct PeakAccum {
    min: f32,
    max: f32,
    n: usize,
}

impl PeakAccum {
    pub fn push(&mut self, s: f32, out: &mut Vec<[i16; 2]>) {
        if self.n == 0 {
            self.min = s;
            self.max = s;
        } else {
            self.min = self.min.min(s);
            self.max = self.max.max(s);
        }
        self.n += 1;
        if self.n == PEAK_BLOCK {
            out.push(to_peak(self.min, self.max));
            self.n = 0;
        }
    }

    pub fn flush(&mut self, out: &mut Vec<[i16; 2]>) {
        if self.n > 0 {
            out.push(to_peak(self.min, self.max));
            self.n = 0;
        }
    }
}

/// Calcule la forme d'onde d'un fichier existant (thread de fond).
pub fn compute_peaks(path: &PathBuf, dst: &Peaks) {
    let Ok(mut src) = WavSource::open(path) else {
        return;
    };
    let mut acc = PeakAccum::default();
    let mut buf = vec![0f32; 65536];
    let mut local = Vec::new();
    loop {
        let n = src.read(&mut buf);
        if n == 0 {
            break;
        }
        for &s in &buf[..n] {
            acc.push(s, &mut local);
        }
        dst.write().extend(&local);
        local.clear();
    }
    acc.flush(&mut local);
    dst.write().extend(&local);
}

/// Lecteur WAV mono séquentiel avec repositionnement.
pub struct WavSource {
    reader: WavReader<BufReader<File>>,
    float: bool,
    scale: f32,
    channels: usize,
    /// Position courante (en trames).
    pos: u64,
    frames: u64,
}

impl WavSource {
    pub fn open(path: &PathBuf) -> hound::Result<Self> {
        let reader = WavReader::open(path)?;
        let spec = reader.spec();
        let frames = reader.duration() as u64;
        Ok(Self {
            float: spec.sample_format == SampleFormat::Float,
            scale: 1.0 / (1u64 << (spec.bits_per_sample.max(1) - 1)) as f32,
            channels: spec.channels.max(1) as usize,
            reader,
            pos: 0,
            frames,
        })
    }

    pub fn seek(&mut self, frame: u64) {
        let frame = frame.min(self.frames);
        if frame != self.pos && self.reader.seek(frame as u32).is_ok() {
            self.pos = frame;
        }
    }

    /// Lit jusqu'à `out.len()` trames (premier canal). Retourne le nombre lu.
    pub fn read(&mut self, out: &mut [f32]) -> usize {
        let want = out.len().min((self.frames - self.pos) as usize);
        let total = want * self.channels;
        let mut n = 0;
        if self.float {
            for (i, s) in self.reader.samples::<f32>().take(total).enumerate() {
                if i % self.channels == 0 {
                    out[n] = s.unwrap_or(0.0);
                    n += 1;
                }
            }
        } else {
            let scale = self.scale;
            for (i, s) in self.reader.samples::<i32>().take(total).enumerate() {
                if i % self.channels == 0 {
                    out[n] = s.unwrap_or(0) as f32 * scale;
                    n += 1;
                }
            }
        }
        self.pos += n as u64;
        n
    }
}

struct ClipSource {
    clip: Clip,
    path: PathBuf,
    src: Option<WavSource>,
    failed: bool,
}

/// Rend le signal d'une piste (mono) à n'importe quelle position de la timeline.
pub struct TrackRenderer {
    clips: Vec<ClipSource>,
    tmp: Vec<f32>,
}

impl TrackRenderer {
    pub fn new(project: &Project, track_idx: usize) -> Self {
        let clips = project.tracks[track_idx]
            .clips
            .iter()
            .map(|c| ClipSource {
                clip: c.clone(),
                path: project.clip_path(c),
                src: None,
                failed: false,
            })
            .collect();
        Self {
            clips,
            tmp: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.clips.is_empty()
    }

    /// Écrit dans `out` le signal de la piste pour `[pos, pos + out.len())`.
    /// Les prises les plus récentes recouvrent les plus anciennes.
    pub fn render(&mut self, pos: u64, out: &mut [f32]) {
        out.fill(0.0);
        let end = pos + out.len() as u64;
        for cs in &mut self.clips {
            let c = &cs.clip;
            if c.end() <= pos || c.start >= end || cs.failed {
                continue;
            }
            if cs.src.is_none() {
                match WavSource::open(&cs.path) {
                    Ok(s) => cs.src = Some(s),
                    Err(_) => {
                        cs.failed = true;
                        continue;
                    }
                }
            }
            let src = cs.src.as_mut().unwrap();
            let from = pos.max(c.start);
            let to = end.min(c.end());
            let n = (to - from) as usize;
            let dst_off = (from - pos) as usize;
            src.seek(c.offset + (from - c.start));
            self.tmp.resize(n, 0.0);
            let got = src.read(&mut self.tmp[..n]);
            out[dst_off..dst_off + got].copy_from_slice(&self.tmp[..got]);
            out[dst_off + got..dst_off + n].fill(0.0);
        }
    }
}
