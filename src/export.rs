//! Export : pistes séparées (stems) et/ou mix stéréo, avec conversion de fréquence
//! et de résolution (16/24/32 bits entiers, 32 bits flottant).

use crate::project::{Project, sanitize};
use crate::render::TrackRenderer;
use anyhow::{Context, Result, bail};
use hound::{WavSpec, WavWriter};
use rubato::{FftFixedIn, Resampler};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::JoinHandle;

const CHUNK: usize = 8192;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum BitDepth {
    I16,
    I24,
    I32,
    F32,
}

impl BitDepth {
    pub const ALL: [BitDepth; 4] = [BitDepth::I16, BitDepth::I24, BitDepth::I32, BitDepth::F32];

    pub fn label(self) -> &'static str {
        match self {
            BitDepth::I16 => "16 bits",
            BitDepth::I24 => "24 bits",
            BitDepth::I32 => "32 bits entier",
            BitDepth::F32 => "32 bits flottant",
        }
    }

    fn spec(self, channels: u16, sample_rate: u32) -> WavSpec {
        let (bits, fmt) = match self {
            BitDepth::I16 => (16, hound::SampleFormat::Int),
            BitDepth::I24 => (24, hound::SampleFormat::Int),
            BitDepth::I32 => (32, hound::SampleFormat::Int),
            BitDepth::F32 => (32, hound::SampleFormat::Float),
        };
        WavSpec {
            channels,
            sample_rate,
            bits_per_sample: bits,
            sample_format: fmt,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ExportSettings {
    pub sample_rate: u32,
    pub depth: BitDepth,
    pub stems: bool,
    pub mix: bool,
    /// Dither TPDF lors du passage en 16 bits.
    pub dither: bool,
    pub dir: PathBuf,
    /// Pistes à exporter (identifiants). Vide = toutes.
    #[serde(default)]
    pub tracks: Vec<u64>,
    /// Plage (début, fin) en échantillons ; `None` = toute la session.
    #[serde(default)]
    pub range: Option<(u64, u64)>,
}

pub struct ExportJob {
    /// Avancement en millièmes.
    pub progress: Arc<AtomicU32>,
    pub cancel: Arc<AtomicBool>,
    handle: Option<JoinHandle<Result<Vec<PathBuf>>>>,
}

impl ExportJob {
    pub fn start(project: &Project, settings: ExportSettings) -> Self {
        let progress = Arc::new(AtomicU32::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let (p2, c2) = (progress.clone(), cancel.clone());
        let project = project.clone();
        let handle = std::thread::Builder::new()
            .name("tracksplus-export".into())
            .spawn(move || run(&project, &settings, &p2, &c2))
            .expect("thread d'export");
        Self {
            progress,
            cancel,
            handle: Some(handle),
        }
    }

    pub fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(|h| h.is_finished())
    }

    pub fn join(mut self) -> Result<Vec<PathBuf>> {
        match self.handle.take().map(|h| h.join()) {
            Some(Ok(r)) => r,
            _ => bail!("L'export a échoué"),
        }
    }
}

fn run(
    project: &Project,
    s: &ExportSettings,
    progress: &AtomicU32,
    cancel: &AtomicBool,
) -> Result<Vec<PathBuf>> {
    let (start, end) = s.range.unwrap_or((0, project.end()));
    let end = end.min(project.end().max(start));
    if end <= start {
        bail!("Rien à exporter : la plage est vide");
    }
    let len = end - start;
    std::fs::create_dir_all(&s.dir).with_context(|| format!("Création de {}", s.dir.display()))?;

    let with_audio: Vec<usize> = (0..project.tracks.len())
        .filter(|&i| !project.tracks[i].clips.is_empty())
        .filter(|&i| s.tracks.is_empty() || s.tracks.contains(&project.tracks[i].id))
        .collect();
    if with_audio.is_empty() {
        bail!("Aucune piste sélectionnée ne contient d'audio");
    }
    let units = if s.stems { with_audio.len() } else { 0 } + usize::from(s.mix);
    if units == 0 {
        bail!("Choisissez au moins « Pistes séparées » ou « Mix stéréo »");
    }
    // Un WAV ne dépasse pas 4 Go : on refuse avant d'écrire plutôt que de produire un
    // fichier illisible.
    let mut widest = 0usize;
    if s.stems {
        widest = with_audio
            .iter()
            .map(|&i| project.tracks[i].channels())
            .max()
            .unwrap_or(0);
    }
    if s.mix {
        widest = widest.max(2);
    }
    check_wav_size(len, project.sample_rate, s.sample_rate, widest, s.depth)?;
    let mut done_units = 0usize;
    let mut out = Vec::new();

    if s.stems {
        for &i in &with_audio {
            let num = i + 1;
            let name = format!("{:02} - {}.wav", num, sanitize(&project.tracks[i].name));
            let path = s.dir.join(name);
            let r = TrackRenderer::new(project, i);
            // Piste stéréo : fichier stéréo ; piste mono : fichier mono.
            let ch = r.channels();
            export_one(project, vec![r], ch, start, len, s, &path, |f| {
                let p = (done_units as f64 + f) / units as f64;
                progress.store((p * 1000.0) as u32, Ordering::Relaxed);
                !cancel.load(Ordering::Relaxed)
            })?;
            out.push(path);
            done_units += 1;
        }
    }
    if s.mix {
        let path = s.dir.join(format!("{} - Mix.wav", sanitize(&project.name)));
        let renderers = with_audio
            .iter()
            .map(|&i| TrackRenderer::new(project, i))
            .collect();
        export_one(project, renderers, 2, start, len, s, &path, |f| {
            let p = (done_units as f64 + f) / units as f64;
            progress.store((p * 1000.0) as u32, Ordering::Relaxed);
            !cancel.load(Ordering::Relaxed)
        })?;
        out.push(path);
    }
    progress.store(1000, Ordering::Relaxed);
    Ok(out)
}

/// Taille maximale des données d'un WAV (champ 32 bits, marge pour l'en-tête).
const MAX_WAV_DATA: u64 = u32::MAX as u64 - 1024;

/// Octets de données d'un fichier exporté : `len` trames à `sr_in` converties en `sr_out`.
fn wav_data_bytes(len: u64, sr_in: u32, sr_out: u32, channels: usize, depth: BitDepth) -> u64 {
    let frames = (len as u128 * sr_out as u128).div_ceil(sr_in.max(1) as u128);
    let bytes_per_sample = match depth {
        BitDepth::I16 => 2,
        BitDepth::I24 => 3,
        BitDepth::I32 | BitDepth::F32 => 4,
    };
    (frames * channels as u128 * bytes_per_sample).min(u64::MAX as u128) as u64
}

/// Refuse un export dont un fichier dépasserait la limite de 4 Go du format WAV.
fn check_wav_size(
    len: u64,
    sr_in: u32,
    sr_out: u32,
    channels: usize,
    depth: BitDepth,
) -> Result<()> {
    let bytes = wav_data_bytes(len, sr_in, sr_out, channels, depth);
    if bytes > MAX_WAV_DATA {
        bail!(
            "Export impossible : un fichier ferait {:.1} Go, au-delà de la limite de 4 Go du \
             format WAV. Choisissez moins de bits, une fréquence plus basse, des pistes \
             séparées plutôt que le mix stéréo, ou une plage plus courte.",
            bytes as f64 / 1e9
        );
    }
    Ok(())
}

/// Quantifie et écrit les échantillons (avec dither TPDF optionnel en 16 bits).
struct SampleWriter {
    w: WavWriter<BufWriter<File>>,
    depth: BitDepth,
    dither: bool,
    rng: u64,
}

impl SampleWriter {
    fn rand(&mut self) -> f32 {
        // xorshift64*
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        let v = self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D);
        (v >> 40) as f32 / (1u64 << 24) as f32
    }

    fn write(&mut self, x: f32) -> Result<()> {
        let x = x.clamp(-1.0, 1.0);
        match self.depth {
            BitDepth::I16 => {
                let mut v = x * 32767.0;
                if self.dither {
                    v += self.rand() - self.rand();
                }
                self.w
                    .write_sample(v.round().clamp(-32768.0, 32767.0) as i16)?;
            }
            BitDepth::I24 => self.w.write_sample((x * 8_388_607.0).round() as i32)?,
            BitDepth::I32 => self
                .w
                .write_sample((x as f64 * 2_147_483_647.0).round() as i32)?,
            BitDepth::F32 => self.w.write_sample(x)?,
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn export_one(
    project: &Project,
    mut renderers: Vec<TrackRenderer>,
    channels: usize,
    offset: u64,
    len: u64,
    s: &ExportSettings,
    path: &Path,
    mut on_progress: impl FnMut(f64) -> bool,
) -> Result<()> {
    let sr_in = project.sample_rate;
    let sr_out = s.sample_rate;
    let spec = s.depth.spec(channels as u16, sr_out);
    let mut w = SampleWriter {
        w: WavWriter::create(path, spec)
            .with_context(|| format!("Création de {}", path.display()))?,
        depth: s.depth,
        dither: s.dither,
        rng: 0x9E37_79B9_7F4A_7C15,
    };

    let mut resampler = if sr_in != sr_out {
        Some(FftFixedIn::<f32>::new(
            sr_in as usize,
            sr_out as usize,
            CHUNK,
            2,
            channels,
        )?)
    } else {
        None
    };
    let expected = (len as u128 * sr_out as u128).div_ceil(sr_in as u128) as u64;
    let mut skip = resampler.as_ref().map_or(0, |r| r.output_delay());
    let mut written = 0u64;

    let mut tbuf = vec![0f32; CHUNK * 2];
    let mut input: Vec<Vec<f32>> = vec![vec![0f32; CHUNK]; channels];
    let mut pos = 0u64;

    let emit = |w: &mut SampleWriter,
                block: &[Vec<f32>],
                skip: &mut usize,
                written: &mut u64|
     -> Result<()> {
        let frames = block[0].len();
        let start = (*skip).min(frames);
        *skip -= start;
        for f in start..frames {
            if *written >= expected {
                break;
            }
            for ch in block {
                w.write(ch[f])?;
            }
            *written += 1;
        }
        Ok(())
    };

    while pos < len {
        let n = ((len - pos) as usize).min(CHUNK);
        for ch in &mut input {
            ch.clear();
            ch.resize(n, 0.0);
        }
        for r in &mut renderers {
            let rc = r.channels();
            let buf = &mut tbuf[..n * rc];
            r.render(offset + pos, buf);
            for (j, ch) in input.iter_mut().enumerate() {
                for (f, d) in ch.iter_mut().enumerate() {
                    *d += match (rc, channels) {
                        // Mono : même signal sur chaque canal de sortie (au centre).
                        (1, _) => buf[f],
                        // Stéréo vers mono : moyenne des deux canaux.
                        (_, 1) => 0.5 * (buf[f * 2] + buf[f * 2 + 1]),
                        _ => buf[f * 2 + j.min(1)],
                    };
                }
            }
        }
        pos += n as u64;
        match &mut resampler {
            None => emit(&mut w, &input, &mut skip, &mut written)?,
            Some(rs) => {
                let block = if n == CHUNK {
                    rs.process(&input, None)?
                } else {
                    rs.process_partial(Some(&input), None)?
                };
                emit(&mut w, &block, &mut skip, &mut written)?;
            }
        }
        if !on_progress(pos as f64 / len as f64) {
            drop(w);
            let _ = std::fs::remove_file(path);
            bail!("Export annulé");
        }
    }
    // Vide le résampleur (latence du filtre).
    if let Some(rs) = &mut resampler {
        let mut guard = 0;
        while written < expected && guard < 64 {
            let block = rs.process_partial::<Vec<f32>>(None, None)?;
            emit(&mut w, &block, &mut skip, &mut written)?;
            guard += 1;
        }
    }
    w.w.finalize()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::Clip;

    fn make_project(tag: &str, sr: u32, frames: usize) -> Project {
        let dir = std::env::temp_dir().join(format!("tp-export-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut p = Project::new("Exp".into(), dir, sr);
        p.ensure_dirs().unwrap();
        let id = p.add_track(Some("Kick".into()), Some(0));
        let spec = WavSpec {
            channels: 1,
            sample_rate: sr,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = WavWriter::create(p.audio_dir().join("k.wav"), spec).unwrap();
        for i in 0..frames {
            let t = i as f32 / sr as f32;
            w.write_sample(0.5 * (2.0 * std::f32::consts::PI * 1000.0 * t).sin())
                .unwrap();
        }
        w.finalize().unwrap();
        p.track_mut(id).unwrap().clips.push(Clip {
            id: 1,
            file: "k.wav".into(),
            start: 100,
            offset: 0,
            len: frames as u64,
            src_len: frames as u64,
            fade_in: 0,
            fade_out: 0,
        });
        p
    }

    fn settings(p: &Project, sr: u32, depth: BitDepth) -> ExportSettings {
        ExportSettings {
            sample_rate: sr,
            depth,
            stems: true,
            mix: true,
            dither: true,
            dir: p.dir.join("Export"),
            tracks: Vec::new(),
            range: None,
        }
    }

    #[test]
    fn export_same_rate_is_exact_length() {
        let p = make_project("same", 48000, 30000);
        let files = run(
            &p,
            &settings(&p, 48000, BitDepth::I24),
            &AtomicU32::new(0),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(files.len(), 2);
        let r = hound::WavReader::open(&files[0]).unwrap();
        assert_eq!(r.duration(), 30100);
        assert_eq!(r.spec().bits_per_sample, 24);
        let m = hound::WavReader::open(&files[1]).unwrap();
        assert_eq!(m.spec().channels, 2);
        std::fs::remove_dir_all(&p.dir).ok();
    }

    #[test]
    fn wav_size_limit_is_checked() {
        // 1 h stéréo 48 kHz 24 bits ≈ 1,04 Go : accepté.
        let hour = 3600 * 48000;
        assert_eq!(
            wav_data_bytes(hour, 48000, 48000, 2, BitDepth::I24),
            hour * 2 * 3
        );
        assert!(check_wav_size(hour, 48000, 48000, 2, BitDepth::I24).is_ok());
        // 4 h stéréo 48 kHz 32 bits ≈ 5,5 Go : refusé, avec des pistes à suivre.
        let e = check_wav_size(4 * hour, 48000, 48000, 2, BitDepth::F32).unwrap_err();
        assert!(e.to_string().contains("4 Go"), "{e}");
        // En 16 bits mono, ça passe.
        assert!(check_wav_size(4 * hour, 48000, 48000, 1, BitDepth::I16).is_ok());
        // Rééchantillonnage vers 96 kHz : la taille double.
        assert_eq!(
            wav_data_bytes(hour, 48000, 96000, 1, BitDepth::I16),
            hour * 2 * 2
        );
        // Juste à la limite.
        let max_frames = MAX_WAV_DATA / 4;
        assert!(check_wav_size(max_frames, 48000, 48000, 1, BitDepth::F32).is_ok());
        assert!(check_wav_size(max_frames + 1, 48000, 48000, 1, BitDepth::F32).is_err());
    }

    #[test]
    fn export_resampled_length_and_level() {
        let p = make_project("rs", 48000, 48000);
        let files = run(
            &p,
            &settings(&p, 44100, BitDepth::I16),
            &AtomicU32::new(0),
            &AtomicBool::new(false),
        )
        .unwrap();
        let mut r = hound::WavReader::open(&files[0]).unwrap();
        assert_eq!(r.spec().sample_rate, 44100);
        let expected = (48100u64 * 44100).div_ceil(48000);
        assert_eq!(r.duration() as u64, expected);
        // Le sinus à 0.5 doit garder son amplitude (≈16383) au milieu du fichier.
        let s: Vec<i16> = r.samples::<i16>().map(|x| x.unwrap()).collect();
        let peak = s[10000..30000]
            .iter()
            .map(|v| v.unsigned_abs())
            .max()
            .unwrap();
        assert!((15500..17200).contains(&peak), "peak = {peak}");
        // Le début (avant la prise) reste silencieux (pas de décalage de latence).
        let first = s.iter().position(|v| v.abs() > 300).unwrap();
        assert!((88..100).contains(&first), "first = {first}");
        std::fs::remove_dir_all(&p.dir).ok();
    }
}

#[cfg(test)]
mod range_tests {
    use super::*;
    use crate::project::Clip;

    #[test]
    fn export_range_and_selection() {
        let dir = std::env::temp_dir().join(format!("tp-export-range-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut p = Project::new("R".into(), dir.clone(), 48000);
        p.ensure_dirs().unwrap();
        let spec = WavSpec {
            channels: 1,
            sample_rate: 48000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = WavWriter::create(p.audio_dir().join("a.wav"), spec).unwrap();
        for i in 0..10000 {
            w.write_sample(i as f32 / 10000.0).unwrap();
        }
        w.finalize().unwrap();
        let a = p.add_track(Some("A".into()), Some(0));
        let b = p.add_track(Some("B".into()), Some(1));
        for id in [a, b] {
            p.track_mut(id).unwrap().clips.push(Clip {
                id: id + 100,
                file: "a.wav".into(),
                start: 0,
                offset: 0,
                len: 10000,
                src_len: 10000,
                fade_in: 0,
                fade_out: 0,
            });
        }
        let s = ExportSettings {
            sample_rate: 48000,
            depth: BitDepth::F32,
            stems: true,
            mix: false,
            dither: false,
            dir: dir.join("Export"),
            tracks: vec![b],
            range: Some((2000, 5000)),
        };
        let files = run(&p, &s, &AtomicU32::new(0), &AtomicBool::new(false)).unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("02 - B.wav"));
        let mut r = hound::WavReader::open(&files[0]).unwrap();
        assert_eq!(r.duration(), 3000);
        let first: f32 = r.samples::<f32>().next().unwrap().unwrap();
        assert!((first - 0.2).abs() < 1e-4);
        std::fs::remove_dir_all(dir).ok();
    }

    /// Piste stéréo : stem stéréo ; le mix additionne stéréo et mono (au centre).
    #[test]
    fn export_stereo_stem_and_mix() {
        let dir = std::env::temp_dir().join(format!("tp-export-st-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut p = Project::new("St".into(), dir.clone(), 48000);
        p.ensure_dirs().unwrap();
        let spec = |channels| WavSpec {
            channels,
            sample_rate: 48000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = WavWriter::create(p.audio_dir().join("st.wav"), spec(2)).unwrap();
        for _ in 0..1000 {
            w.write_sample(0.4f32).unwrap();
            w.write_sample(-0.4f32).unwrap();
        }
        w.finalize().unwrap();
        let mut w = WavWriter::create(p.audio_dir().join("mo.wav"), spec(1)).unwrap();
        for _ in 0..1000 {
            w.write_sample(0.1f32).unwrap();
        }
        w.finalize().unwrap();
        let st = p.add_track_ch(Some("Synthé".into()), Some(0), true);
        let mo = p.add_track(Some("Voix".into()), Some(2));
        for (id, file) in [(st, "st.wav"), (mo, "mo.wav")] {
            p.track_mut(id).unwrap().clips.push(Clip {
                id: id + 100,
                file: file.into(),
                start: 0,
                offset: 0,
                len: 1000,
                src_len: 1000,
                fade_in: 0,
                fade_out: 0,
            });
        }
        let s = ExportSettings {
            sample_rate: 48000,
            depth: BitDepth::F32,
            stems: true,
            mix: true,
            dither: false,
            dir: dir.join("Export"),
            tracks: Vec::new(),
            range: None,
        };
        let files = run(&p, &s, &AtomicU32::new(0), &AtomicBool::new(false)).unwrap();
        assert_eq!(files.len(), 3);
        let read = |f: &PathBuf| {
            let mut r = hound::WavReader::open(f).unwrap();
            let ch = r.spec().channels;
            let v: Vec<f32> = r.samples::<f32>().map(|x| x.unwrap()).collect();
            (ch, v)
        };
        let (ch, v) = read(&files[0]);
        assert_eq!(ch, 2, "stem stéréo");
        assert_eq!(v.len(), 2000);
        assert_eq!(&v[..2], &[0.4, -0.4]);
        let (ch, v) = read(&files[1]);
        assert_eq!(ch, 1, "stem mono");
        assert_eq!(v[0], 0.1);
        let (ch, v) = read(&files[2]);
        assert_eq!(ch, 2);
        assert!(
            (v[0] - 0.5).abs() < 1e-6 && (v[1] + 0.3).abs() < 1e-6,
            "{:?}",
            &v[..2]
        );
        std::fs::remove_dir_all(dir).ok();
    }
}
