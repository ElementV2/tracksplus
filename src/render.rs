//! Lecture des prises depuis le disque (lecture / export) et calcul des formes d'onde.

use crate::project::{Clip, Project};
use hound::{SampleFormat, WavReader};
use parking_lot::RwLock;
use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;
use std::sync::Arc;

/// Nombre d'échantillons résumés par un point de forme d'onde.
pub const PEAK_BLOCK: usize = 1024;

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
/// Compte en trames : en stéréo, la forme d'onde couvre les deux canaux.
#[derive(Default)]
pub struct PeakAccum {
    min: f32,
    max: f32,
    /// Au moins un échantillon depuis le dernier point émis.
    started: bool,
    n: usize,
}

impl PeakAccum {
    /// Ajoute un échantillon de la trame en cours (sans la terminer).
    pub fn add(&mut self, s: f32) {
        if self.started {
            self.min = self.min.min(s);
            self.max = self.max.max(s);
        } else {
            self.min = s;
            self.max = s;
            self.started = true;
        }
    }

    /// Termine la trame en cours.
    pub fn end_frame(&mut self, out: &mut Vec<[i16; 2]>) {
        self.n += 1;
        if self.n == PEAK_BLOCK {
            out.push(to_peak(self.min, self.max));
            self.n = 0;
            self.started = false;
        }
    }

    pub fn flush(&mut self, out: &mut Vec<[i16; 2]>) {
        if self.n > 0 {
            out.push(to_peak(self.min, self.max));
            self.n = 0;
            self.started = false;
        }
    }
}

/// Calcule la forme d'onde d'un fichier existant (thread de fond).
pub fn compute_peaks(path: &PathBuf, dst: &Peaks) {
    let Ok(mut src) = WavSource::open(path) else {
        return;
    };
    let ch = src.channels().min(2);
    let mut acc = PeakAccum::default();
    let mut buf = vec![0f32; 65536 * ch];
    let mut local = Vec::new();
    loop {
        let n = src.read_frames(&mut buf, ch);
        if n == 0 {
            break;
        }
        for f in buf[..n * ch].chunks_exact(ch) {
            for &s in f {
                acc.add(s);
            }
            acc.end_frame(&mut local);
        }
        dst.write().extend(&local);
        local.clear();
    }
    acc.flush(&mut local);
    dst.write().extend(&local);
}

/// Lecteur WAV séquentiel (mono ou multicanal) avec repositionnement.
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
        if frame != self.pos {
            if self.reader.seek(frame as u32).is_ok() {
                self.pos = frame;
            } else {
                // Échec de lecture : silence plutôt qu'un mauvais passage audio.
                self.pos = self.frames;
            }
        }
    }

    /// Nombre de canaux du fichier.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Lit jusqu'à `out.len() / out_ch` trames, entrelacées sur `out_ch` canaux (1 ou 2).
    /// Le fichier est adapté : mono → stéréo par duplication, stéréo → mono par moyenne
    /// des deux premiers canaux. Retourne le nombre de trames lues.
    pub fn read_frames(&mut self, out: &mut [f32], out_ch: usize) -> usize {
        let out_ch = out_ch.clamp(1, 2);
        let fc = self.channels;
        let want = (out.len() / out_ch).min((self.frames - self.pos) as usize);
        let total = want * fc;
        let float = self.float;
        let scale = self.scale;
        // Échantillons du fichier, convertis en f32 (une erreur de lecture = silence).
        let samples: Box<dyn Iterator<Item = f32> + '_> = if float {
            Box::new(
                self.reader
                    .samples::<f32>()
                    .take(total)
                    .map(|s| s.unwrap_or(0.0)),
            )
        } else {
            Box::new(
                self.reader
                    .samples::<i32>()
                    .take(total)
                    .map(move |s| s.unwrap_or(0) as f32 * scale),
            )
        };
        let mut n = 0;
        let mut l = 0f32;
        for (i, s) in samples.enumerate() {
            let k = i % fc;
            match (fc, out_ch) {
                (1, 1) => out[n] = s,
                (1, _) => {
                    out[n * 2] = s;
                    out[n * 2 + 1] = s;
                }
                (_, 1) if k == 0 => l = s,
                (_, 1) if k == 1 => out[n] = 0.5 * (l + s),
                (_, _) if k < 2 => out[n * 2 + k] = s,
                _ => {}
            }
            if k == fc - 1 {
                n += 1;
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

/// Rend le signal d'une piste (mono, ou stéréo entrelacé) à n'importe quelle position
/// de la timeline.
pub struct TrackRenderer {
    clips: Vec<ClipSource>,
    tmp: Vec<f32>,
    channels: usize,
}

impl TrackRenderer {
    pub fn new(project: &Project, track_idx: usize) -> Self {
        let track = &project.tracks[track_idx];
        let clips = track
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
            channels: track.channels(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.clips.is_empty()
    }

    /// Canaux rendus par `render` (1 = mono, 2 = stéréo entrelacé).
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Écrit dans `out` le signal de la piste pour `[pos, pos + out.len() / channels)`,
    /// entrelacé sur `channels()` canaux. Les prises récentes recouvrent les anciennes.
    pub fn render(&mut self, pos: u64, out: &mut [f32]) {
        out.fill(0.0);
        let ch = self.channels;
        let end = pos + (out.len() / ch) as u64;
        // Fichiers ouverts uniquement autour de la position de lecture : ouverture un peu
        // en avance (pas de trou au début d'une prise), fermeture une fois la prise passée.
        // Le nombre de fichiers ouverts reste ainsi borné, même avec des centaines de prises.
        const LOOKAHEAD: u64 = 96_000;
        for cs in &mut self.clips {
            let c = &cs.clip;
            if c.end() <= pos || c.start > end + LOOKAHEAD {
                cs.src = None;
            } else if cs.src.is_none() && !cs.failed && c.start >= end {
                match WavSource::open(&cs.path) {
                    Ok(s) => cs.src = Some(s),
                    Err(_) => cs.failed = true,
                }
            }
        }
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
            self.tmp.resize(n * ch, 0.0);
            let got = src.read_frames(&mut self.tmp[..n * ch], ch);
            self.tmp[got * ch..n * ch].fill(0.0);
            let dst = &mut out[dst_off * ch..(dst_off + n) * ch];
            if c.fade_in == 0 && c.fade_out == 0 {
                dst.copy_from_slice(&self.tmp[..n * ch]);
            } else {
                // Fondu : la prise recouvre ce qui est dessous en proportion de son gain,
                // ce qui fait un fondu enchaîné naturel entre prises qui se chevauchent.
                for k in 0..n {
                    let g = c.fade_gain(from + k as u64);
                    for j in 0..ch {
                        let o = &mut dst[k * ch + j];
                        *o = *o * (1.0 - g) + self.tmp[k * ch + j] * g;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::{Clip, Project};

    #[test]
    fn fades_and_crossfade() {
        let dir = std::env::temp_dir().join(format!("tp-render-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut p = Project::new("F".into(), dir.clone(), 48000);
        p.ensure_dirs().unwrap();
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        for (name, v) in [("a.wav", 1.0f32), ("b.wav", 0.5)] {
            let mut w = hound::WavWriter::create(p.audio_dir().join(name), spec).unwrap();
            for _ in 0..2000 {
                w.write_sample(v).unwrap();
            }
            w.finalize().unwrap();
        }
        let t = p.add_track(None, Some(0));
        let mk = |id, file: &str, start, fade_in, fade_out| Clip {
            id,
            file: file.into(),
            start,
            offset: 0,
            len: 2000,
            src_len: 2000,
            fade_in,
            fade_out,
        };
        p.track_mut(t)
            .unwrap()
            .clips
            .push(mk(1, "a.wav", 0, 400, 0));
        // Prise B par-dessus à partir de 1000, avec un fondu d'entrée de 200 : fondu enchaîné.
        p.track_mut(t)
            .unwrap()
            .clips
            .push(mk(2, "b.wav", 1000, 200, 0));
        let mut r = TrackRenderer::new(&p, 0);
        let mut out = vec![0.0f32; 3000];
        r.render(0, &mut out);
        assert_eq!(out[0], 0.0);
        assert!((out[200] - 0.5).abs() < 0.01, "milieu du fondu d'entrée");
        assert!((out[800] - 1.0).abs() < 1e-6);
        assert!(
            (out[1100] - 0.75).abs() < 0.01,
            "moitié du fondu enchaîné A→B"
        );
        assert!((out[1500] - 0.5).abs() < 1e-6);
        std::fs::remove_dir_all(dir).ok();
    }

    /// Pistes stéréo : fichier stéréo lu tel quel, fichier mono dupliqué sur G/D, et
    /// fichier stéréo posé sur une piste mono ramené à la moyenne des deux canaux.
    #[test]
    fn stereo_tracks_adapt_files() {
        let dir = std::env::temp_dir().join(format!("tp-render-st-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut p = Project::new("S".into(), dir.clone(), 48000);
        p.ensure_dirs().unwrap();
        let spec = |channels| hound::WavSpec {
            channels,
            sample_rate: 48000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(p.audio_dir().join("st.wav"), spec(2)).unwrap();
        for _ in 0..1000 {
            w.write_sample(0.8f32).unwrap();
            w.write_sample(-0.2f32).unwrap();
        }
        w.finalize().unwrap();
        let mut w = hound::WavWriter::create(p.audio_dir().join("mo.wav"), spec(1)).unwrap();
        for _ in 0..1000 {
            w.write_sample(0.5f32).unwrap();
        }
        w.finalize().unwrap();
        let clip = |id, file: &str, start| Clip {
            id,
            file: file.into(),
            start,
            offset: 0,
            len: 1000,
            src_len: 1000,
            fade_in: 0,
            fade_out: 0,
        };
        let st = p.add_track_ch(None, Some(0), true);
        p.track_mut(st).unwrap().clips.push(clip(1, "st.wav", 0));
        p.track_mut(st).unwrap().clips.push(clip(2, "mo.wav", 2000));
        let mo = p.add_track(None, Some(2));
        p.track_mut(mo).unwrap().clips.push(clip(3, "st.wav", 0));

        let mut r = TrackRenderer::new(&p, 0);
        assert_eq!(r.channels(), 2);
        let mut out = vec![9.0f32; 3000 * 2];
        r.render(0, &mut out);
        assert_eq!(&out[..2], &[0.8, -0.2]);
        assert_eq!(&out[1998..2000], &[0.8, -0.2]);
        assert_eq!(&out[2000..2002], &[0.0, 0.0]);
        assert_eq!(&out[4000..4002], &[0.5, 0.5], "mono dupliqué");
        // Lecture à partir du milieu : toujours aligné sur des trames entières.
        let mut out = vec![0.0f32; 10];
        r.render(500, &mut out);
        assert!(out.as_chunks::<2>().0.iter().all(|f| *f == [0.8, -0.2]));

        let mut r = TrackRenderer::new(&p, 1);
        assert_eq!(r.channels(), 1);
        let mut out = vec![0.0f32; 1000];
        r.render(0, &mut out);
        assert!(out.iter().all(|&x| (x - 0.3).abs() < 1e-6), "moyenne G/D");
        std::fs::remove_dir_all(dir).ok();
    }
}
