//! Export : pistes séparées (stems) et/ou mix (stéréo ou mono), avec conversion de
//! fréquence et de résolution, dans tous les formats courants : WAV, BWF (RF64 au-delà de
//! 4 Go), AIFF, FLAC, MP3 et Ogg Vorbis. Normalisation crête ou LUFS optionnelle, en
//! deux passes (mesure, puis export avec le gain calculé).

use crate::encode::{self, FileMeta, MAX_AIFF_DATA, SinkSpec};
use crate::project::{Project, sanitize};
use crate::render::TrackRenderer;
use anyhow::{Context, Result, anyhow, bail};
use rubato::{FftFixedIn, Resampler};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::JoinHandle;

const CHUNK: usize = 8192;

/// Fréquences d'échantillonnage proposées à l'export.
pub const SAMPLE_RATES: [u32; 6] = [44100, 48000, 88200, 96000, 176400, 192000];

/// Plafond de crête vraie (dBTP) appliqué par la normalisation LUFS.
pub const TRUE_PEAK_CEILING_DB: f32 = -1.0;

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

    /// Octets par échantillon.
    pub fn bytes(self) -> u64 {
        match self {
            BitDepth::I16 => 2,
            BitDepth::I24 => 3,
            BitDepth::I32 | BitDepth::F32 => 4,
        }
    }
}

/// Format des fichiers exportés.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ExportFormat {
    #[default]
    Wav,
    Bwf,
    Aiff,
    Flac,
    Mp3,
    Ogg,
}

impl ExportFormat {
    pub fn label(self) -> &'static str {
        match self {
            ExportFormat::Wav => "WAV",
            ExportFormat::Bwf => "BWF (Broadcast Wave)",
            ExportFormat::Aiff => "AIFF",
            ExportFormat::Flac => "FLAC (sans perte)",
            ExportFormat::Mp3 => "MP3",
            ExportFormat::Ogg => "Ogg Vorbis",
        }
    }

    /// Extension des fichiers (sans le point).
    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Wav | ExportFormat::Bwf => "wav",
            ExportFormat::Aiff => "aif",
            ExportFormat::Flac => "flac",
            ExportFormat::Mp3 => "mp3",
            ExportFormat::Ogg => "ogg",
        }
    }

    /// Format avec perte (MP3, Ogg) : pas de choix de résolution, 48 kHz maximum.
    pub fn is_lossy(self) -> bool {
        matches!(self, ExportFormat::Mp3 | ExportFormat::Ogg)
    }

    /// Résolutions possibles dans ce format (vide pour les formats avec perte).
    pub fn depths(self) -> &'static [BitDepth] {
        match self {
            ExportFormat::Wav | ExportFormat::Bwf => &BitDepth::ALL,
            ExportFormat::Aiff => &[BitDepth::I16, BitDepth::I24, BitDepth::I32],
            ExportFormat::Flac => &[BitDepth::I16, BitDepth::I24],
            ExportFormat::Mp3 | ExportFormat::Ogg => &[],
        }
    }

    /// Fréquence réellement écrite pour une fréquence demandée : MP3 et Ogg s'arrêtent à
    /// 48 kHz (44,1 kHz pour la famille 44,1 : 88,2 et 176,4 kHz).
    pub fn output_rate(self, requested: u32) -> u32 {
        if self.is_lossy() && requested > 48000 {
            if requested.is_multiple_of(44100) {
                44100
            } else {
                48000
            }
        } else {
            requested
        }
    }

    /// Résolution réellement écrite : la plus proche disponible dans ce format.
    /// `None` pour les formats avec perte.
    pub fn output_depth(self, requested: BitDepth) -> Option<BitDepth> {
        let d = self.depths();
        if d.is_empty() {
            None
        } else if d.contains(&requested) {
            Some(requested)
        } else {
            // F32 en AIFF → 32 bits entier ; 32 bits en FLAC → 24 bits.
            d.last().copied()
        }
    }
}

/// Débit MP3.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Mp3Bitrate {
    Cbr128,
    Cbr192,
    Cbr256,
    #[default]
    Cbr320,
    /// VBR qualité maximale (LAME -V0, ≈ 245 kbit/s).
    V0,
}

impl Mp3Bitrate {
    pub const ALL: [Mp3Bitrate; 5] = [
        Mp3Bitrate::Cbr128,
        Mp3Bitrate::Cbr192,
        Mp3Bitrate::Cbr256,
        Mp3Bitrate::Cbr320,
        Mp3Bitrate::V0,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Mp3Bitrate::Cbr128 => "128 kbit/s",
            Mp3Bitrate::Cbr192 => "192 kbit/s",
            Mp3Bitrate::Cbr256 => "256 kbit/s",
            Mp3Bitrate::Cbr320 => "320 kbit/s",
            Mp3Bitrate::V0 => "VBR V0 (≈ 245 kbit/s)",
        }
    }

    /// Débit constant en kbit/s ; `None` pour le VBR.
    pub fn kbps(self) -> Option<u32> {
        match self {
            Mp3Bitrate::Cbr128 => Some(128),
            Mp3Bitrate::Cbr192 => Some(192),
            Mp3Bitrate::Cbr256 => Some(256),
            Mp3Bitrate::Cbr320 => Some(320),
            Mp3Bitrate::V0 => None,
        }
    }
}

/// Qualité Ogg Vorbis (VBR).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum OggQuality {
    /// Qualité 5 (≈ 160 kbit/s).
    Q5,
    /// Qualité 7 (≈ 224 kbit/s).
    #[default]
    Q7,
    /// Qualité 9 (≈ 320 kbit/s).
    Q9,
}

impl OggQuality {
    pub const ALL: [OggQuality; 3] = [OggQuality::Q5, OggQuality::Q7, OggQuality::Q9];

    pub fn label(self) -> &'static str {
        match self {
            OggQuality::Q5 => "Qualité 5 (≈ 160 kbit/s)",
            OggQuality::Q7 => "Qualité 7 (≈ 224 kbit/s)",
            OggQuality::Q9 => "Qualité 9 (≈ 320 kbit/s)",
        }
    }

    /// Facteur de qualité libvorbis (−0,1 à 1).
    pub fn quality(self) -> f32 {
        match self {
            OggQuality::Q5 => 0.5,
            OggQuality::Q7 => 0.7,
            OggQuality::Q9 => 0.9,
        }
    }
}

/// Normalisation appliquée à chaque fichier exporté.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug, Default)]
pub enum Normalize {
    #[default]
    Off,
    /// Crête d'échantillon ramenée à cette valeur (dBFS, ≤ 0).
    Peak(f32),
    /// Sonie intégrée (EBU R128) ramenée à cette valeur (LUFS), crête vraie limitée à
    /// −1 dBTP (le gain est réduit si besoin, sans limiteur).
    Lufs(f32),
}

impl Normalize {
    pub fn is_off(self) -> bool {
        self == Normalize::Off
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ExportSettings {
    /// Fréquence demandée (MP3/Ogg : plafonnée à 48 kHz, voir [`Self::output_rate`]).
    pub sample_rate: u32,
    /// Résolution demandée (adaptée au format, voir [`Self::output_depth`]).
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
    #[serde(default)]
    pub format: ExportFormat,
    #[serde(default)]
    pub normalize: Normalize,
    /// Mix réduit en mono (fichier mono).
    #[serde(default)]
    pub mix_mono: bool,
    #[serde(default)]
    pub mp3_bitrate: Mp3Bitrate,
    #[serde(default)]
    pub ogg_quality: OggQuality,
}

impl Default for ExportSettings {
    fn default() -> Self {
        Self {
            sample_rate: 48000,
            depth: BitDepth::I24,
            stems: true,
            mix: false,
            dither: true,
            dir: PathBuf::new(),
            tracks: Vec::new(),
            range: None,
            format: ExportFormat::Wav,
            normalize: Normalize::Off,
            mix_mono: false,
            mp3_bitrate: Mp3Bitrate::default(),
            ogg_quality: OggQuality::default(),
        }
    }
}

impl ExportSettings {
    /// Fréquence réellement écrite dans les fichiers.
    pub fn output_rate(&self) -> u32 {
        self.format.output_rate(self.sample_rate)
    }

    /// Résolution réellement écrite (`None` pour MP3/Ogg).
    pub fn output_depth(&self) -> Option<BitDepth> {
        self.format.output_depth(self.depth)
    }

    /// Le dither s'applique-t-il (sortie 16 bits avec dither coché) ?
    pub fn dither_applies(&self) -> bool {
        self.dither && self.output_depth() == Some(BitDepth::I16)
    }

    /// Ramène fréquence et résolution demandées à ce que le format sait écrire (la
    /// fenêtre d'export affiche ainsi toujours le réglage réel).
    pub fn fit_format(&mut self) {
        self.sample_rate = self.output_rate();
        if let Some(d) = self.output_depth() {
            self.depth = d;
        }
    }

    /// Taille estimée d'un fichier de `channels` canaux et `secs` secondes (octets).
    pub fn estimated_bytes(&self, secs: f64, channels: usize) -> f64 {
        let kbps = |k: f64| k * 1000.0 / 8.0 * secs;
        match self.format {
            ExportFormat::Mp3 => kbps(self.mp3_bitrate.kbps().unwrap_or(245) as f64),
            ExportFormat::Ogg => kbps(match self.ogg_quality {
                OggQuality::Q5 => 160.0,
                OggQuality::Q7 => 224.0,
                OggQuality::Q9 => 320.0,
            }),
            f => {
                let depth = self.output_depth().unwrap_or(BitDepth::I24);
                let pcm = secs * self.output_rate() as f64 * channels as f64 * depth.bytes() as f64;
                // Le FLAC divise en général la taille par 1,5 à 2.
                if f == ExportFormat::Flac {
                    pcm * 0.6
                } else {
                    pcm
                }
            }
        }
    }
}

pub struct ExportJob {
    /// Avancement en millièmes (les deux passes de la normalisation comprises).
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

/// Un fichier à produire.
struct Unit {
    path: PathBuf,
    /// Indices des pistes mixées dans ce fichier.
    tracks: Vec<usize>,
    channels: usize,
    meta: FileMeta,
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
    let sr_in = project.sample_rate;
    let sr_out = s.output_rate();
    if !(8000..=384_000).contains(&sr_out) {
        bail!("Fréquence d'export invalide : {sr_out} Hz");
    }
    std::fs::create_dir_all(&s.dir).with_context(|| format!("Création de {}", s.dir.display()))?;

    let with_audio: Vec<usize> = (0..project.tracks.len())
        .filter(|&i| !project.tracks[i].clips.is_empty())
        .filter(|&i| s.tracks.is_empty() || s.tracks.contains(&project.tracks[i].id))
        .collect();
    if with_audio.is_empty() {
        bail!("Aucune piste sélectionnée ne contient d'audio");
    }
    if !s.stems && !s.mix {
        bail!("Cochez au moins « Pistes séparées » ou « Mix »");
    }

    let ext = s.format.extension();
    let time_reference = (start as u128 * sr_out as u128 / sr_in.max(1) as u128) as u64;
    let meta = |title: String| FileMeta {
        description: format!("{} - {}", project.name, title),
        album: project.name.clone(),
        title,
        time_reference,
    };
    let mut units = Vec::new();
    if s.stems {
        for &i in &with_audio {
            let t = &project.tracks[i];
            units.push(Unit {
                path: s
                    .dir
                    .join(format!("{:02} - {}.{ext}", i + 1, sanitize(&t.name))),
                tracks: vec![i],
                // Piste stéréo : fichier stéréo ; piste mono : fichier mono.
                channels: t.channels(),
                meta: meta(t.name.clone()),
            });
        }
    }
    if s.mix {
        units.push(Unit {
            path: s
                .dir
                .join(format!("{} - Mix.{ext}", sanitize(&project.name))),
            tracks: with_audio.clone(),
            channels: if s.mix_mono { 1 } else { 2 },
            meta: meta("Mix".into()),
        });
    }

    // L'AIFF stocke ses tailles sur 32 bits : on refuse avant d'écrire plutôt que de
    // produire un fichier illisible. Le WAV/BWF passe en RF64 au-delà de 4 Go.
    if s.format == ExportFormat::Aiff {
        let widest = units.iter().map(|u| u.channels).max().unwrap_or(1);
        let depth = s.output_depth().unwrap_or(BitDepth::I32);
        check_aiff_size(len, sr_in, sr_out, widest, depth)?;
    }

    let passes = if s.normalize.is_off() { 1 } else { 2 };
    let total = (units.len() * passes) as f64;
    let mut done = 0usize;
    let mut out = Vec::new();
    for u in &units {
        let mut tick = |f: f64| {
            let p = (done as f64 + f.clamp(0.0, 1.0)) / total;
            progress.store((p * 1000.0) as u32, Ordering::Relaxed);
            !cancel.load(Ordering::Relaxed)
        };
        let make = || {
            u.tracks
                .iter()
                .map(|&i| TrackRenderer::new(project, i))
                .collect::<Vec<_>>()
        };
        let gain = match s.normalize {
            Normalize::Off => 1.0,
            n => {
                let m = measure(
                    project,
                    make(),
                    u.channels,
                    start,
                    len,
                    sr_out,
                    n,
                    &mut tick,
                )?;
                done += 1;
                m
            }
        };
        let mut tick = |f: f64| {
            let p = (done as f64 + f.clamp(0.0, 1.0)) / total;
            progress.store((p * 1000.0) as u32, Ordering::Relaxed);
            !cancel.load(Ordering::Relaxed)
        };
        let spec = SinkSpec {
            format: s.format,
            depth: s.output_depth().unwrap_or(BitDepth::F32),
            dither: s.dither,
            channels: u.channels,
            sample_rate: sr_out,
            mp3_bitrate: s.mp3_bitrate,
            ogg_quality: s.ogg_quality,
            meta: &u.meta,
        };
        write_file(project, make(), &spec, &u.path, start, len, gain, &mut tick)?;
        done += 1;
        out.push(u.path.clone());
    }
    progress.store(1000, Ordering::Relaxed);
    Ok(out)
}

/// Trames produites pour `len` trames à `sr_in` converties en `sr_out`.
fn expected_frames(len: u64, sr_in: u32, sr_out: u32) -> u64 {
    (len as u128 * sr_out as u128).div_ceil(sr_in.max(1) as u128) as u64
}

/// Octets de données d'un fichier PCM exporté : `len` trames à `sr_in` converties en `sr_out`.
fn pcm_data_bytes(len: u64, sr_in: u32, sr_out: u32, channels: usize, depth: BitDepth) -> u64 {
    let frames = expected_frames(len, sr_in, sr_out) as u128;
    (frames * channels as u128 * depth.bytes() as u128).min(u64::MAX as u128) as u64
}

/// Refuse un export AIFF dont un fichier dépasserait la limite de 4 Go du format.
fn check_aiff_size(
    len: u64,
    sr_in: u32,
    sr_out: u32,
    channels: usize,
    depth: BitDepth,
) -> Result<()> {
    let bytes = pcm_data_bytes(len, sr_in, sr_out, channels, depth);
    if bytes > MAX_AIFF_DATA {
        bail!(
            "Export impossible : un fichier ferait {:.1} Go, au-delà de la limite de 4 Go du \
             format AIFF. Choisissez le WAV ou le BWF (sans limite de taille), moins de bits, \
             une fréquence plus basse, des pistes séparées plutôt que le mix, ou une plage \
             plus courte.",
            bytes as f64 / 1e9
        );
    }
    Ok(())
}

/// Première passe de la normalisation : mesure le signal exporté et renvoie le gain
/// linéaire à appliquer.
#[allow(clippy::too_many_arguments)]
fn measure(
    project: &Project,
    renderers: Vec<TrackRenderer>,
    channels: usize,
    offset: u64,
    len: u64,
    sr_out: u32,
    mode: Normalize,
    on_progress: &mut impl FnMut(f64) -> bool,
) -> Result<f32> {
    match mode {
        Normalize::Off => Ok(1.0),
        Normalize::Peak(target) => {
            let mut peak = 0f32;
            render(
                project,
                renderers,
                channels,
                offset,
                len,
                sr_out,
                |b| {
                    peak = b.iter().fold(peak, |m, x| m.max(x.abs()));
                    Ok(())
                },
                on_progress,
            )?;
            Ok(peak_gain(peak, target))
        }
        Normalize::Lufs(target) => {
            use ebur128::{EbuR128, Mode};
            let mut meter = EbuR128::new(channels as u32, sr_out, Mode::I | Mode::TRUE_PEAK)
                .map_err(|e| anyhow!("Mesure de sonie : {e:?}"))?;
            render(
                project,
                renderers,
                channels,
                offset,
                len,
                sr_out,
                |b| {
                    meter
                        .add_frames_f32(b)
                        .map_err(|e| anyhow!("Mesure de sonie : {e:?}"))
                },
                on_progress,
            )?;
            let loudness = meter.loudness_global().unwrap_or(f64::NEG_INFINITY);
            let true_peak = (0..channels as u32)
                .filter_map(|c| meter.true_peak(c).ok())
                .fold(0f64, f64::max);
            Ok(lufs_gain(loudness, true_peak, target))
        }
    }
}

fn db_to_gain(db: f64) -> f32 {
    10f64.powf(db / 20.0) as f32
}

/// Gain ramenant la crête `peak` (linéaire) à `target` dBFS.
fn peak_gain(peak: f32, target: f32) -> f32 {
    if peak <= 1e-9 {
        return 1.0;
    }
    let target = target.min(0.0) as f64;
    db_to_gain(target - 20.0 * (peak as f64).log10())
}

/// Gain ramenant la sonie intégrée à `target` LUFS, réduit pour que la crête vraie
/// reste sous −1 dBTP.
fn lufs_gain(loudness: f64, true_peak: f64, target: f32) -> f32 {
    if !loudness.is_finite() {
        // Silence (sous le seuil de mesure) : rien à normaliser.
        return 1.0;
    }
    let mut db = target.clamp(-70.0, 0.0) as f64 - loudness;
    if true_peak > 1e-9 {
        let tp_db = 20.0 * true_peak.log10();
        db = db.min(TRUE_PEAK_CEILING_DB as f64 - tp_db);
    }
    db_to_gain(db)
}

/// Deuxième (ou unique) passe : écrit le fichier avec le gain donné. Le fichier
/// incomplet est supprimé en cas d'erreur ou d'annulation.
#[allow(clippy::too_many_arguments)]
fn write_file(
    project: &Project,
    renderers: Vec<TrackRenderer>,
    spec: &SinkSpec,
    path: &Path,
    offset: u64,
    len: u64,
    gain: f32,
    on_progress: &mut impl FnMut(f64) -> bool,
) -> Result<()> {
    let mut sink = encode::create(path, spec)?;
    let mut scaled = Vec::new();
    let r = render(
        project,
        renderers,
        spec.channels,
        offset,
        len,
        spec.sample_rate,
        |b| {
            if gain == 1.0 {
                sink.write(b)
            } else {
                scaled.clear();
                scaled.extend(b.iter().map(|x| x * gain));
                sink.write(&scaled)
            }
        },
        on_progress,
    )
    .and_then(|()| sink.finish());
    if r.is_err() {
        let _ = std::fs::remove_file(path);
    }
    r
}

/// Rend `len` trames depuis `offset` (mixées sur `channels` canaux), les rééchantillonne
/// à `sr_out` et passe des blocs entrelacés à `sink`. `on_progress(f)` renvoie `false`
/// pour annuler.
#[allow(clippy::too_many_arguments)]
fn render(
    project: &Project,
    mut renderers: Vec<TrackRenderer>,
    channels: usize,
    offset: u64,
    len: u64,
    sr_out: u32,
    mut sink: impl FnMut(&[f32]) -> Result<()>,
    on_progress: &mut impl FnMut(f64) -> bool,
) -> Result<()> {
    let sr_in = project.sample_rate;
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
    let expected = expected_frames(len, sr_in, sr_out);
    let mut skip = resampler.as_ref().map_or(0, |r| r.output_delay());
    let mut written = 0u64;

    let mut tbuf = vec![0f32; CHUNK * 2];
    let mut input: Vec<Vec<f32>> = vec![vec![0f32; CHUNK]; channels];
    let mut inter: Vec<f32> = Vec::new();
    let mut pos = 0u64;

    let mut emit = |block: &[Vec<f32>], skip: &mut usize, written: &mut u64| -> Result<()> {
        let frames = block[0].len();
        let start = (*skip).min(frames);
        *skip -= start;
        let take = ((frames - start) as u64).min(expected - *written) as usize;
        if take == 0 {
            return Ok(());
        }
        inter.clear();
        for f in start..start + take {
            for ch in block {
                inter.push(ch[f]);
            }
        }
        *written += take as u64;
        sink(&inter)
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
            None => emit(&input, &mut skip, &mut written)?,
            Some(rs) => {
                let block = if n == CHUNK {
                    rs.process(&input, None)?
                } else {
                    rs.process_partial(Some(&input), None)?
                };
                emit(&block, &mut skip, &mut written)?;
            }
        }
        if !on_progress(pos as f64 / len as f64) {
            bail!("Export annulé");
        }
    }
    // Vide le résampleur (latence du filtre).
    if let Some(rs) = &mut resampler {
        let mut guard = 0;
        while written < expected && guard < 64 {
            let block = rs.process_partial::<Vec<f32>>(None, None)?;
            emit(&block, &mut skip, &mut written)?;
            guard += 1;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::Clip;
    use hound::{WavSpec, WavWriter};

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
            ..Default::default()
        }
    }

    fn export(p: &Project, s: &ExportSettings) -> Vec<PathBuf> {
        run(p, s, &AtomicU32::new(0), &AtomicBool::new(false)).unwrap()
    }

    /// Décode un fichier avec symphonia : (fréquence, canaux, échantillons entrelacés).
    fn decode(path: &Path) -> (u32, usize, Vec<f32>) {
        use symphonia::core::audio::SampleBuffer;
        use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
        use symphonia::core::errors::Error as SymError;
        use symphonia::core::io::MediaSourceStream;
        use symphonia::core::probe::Hint;
        let file = std::fs::File::open(path).unwrap();
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        hint.with_extension(path.extension().unwrap().to_str().unwrap());
        let probed = symphonia::default::get_probe()
            .format(&hint, mss, &Default::default(), &Default::default())
            .unwrap();
        let mut format = probed.format;
        let track = format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
            .unwrap()
            .clone();
        let mut dec = symphonia::default::get_codecs()
            .make(&track.codec_params, &DecoderOptions::default())
            .unwrap();
        let mut out = Vec::new();
        let mut rate = 0;
        let mut ch = 0;
        loop {
            let packet = match format.next_packet() {
                Ok(p) => p,
                Err(SymError::IoError(_)) => break,
                Err(e) => panic!("{e}"),
            };
            if packet.track_id() != track.id {
                continue;
            }
            let buf = dec.decode(&packet).unwrap();
            rate = buf.spec().rate;
            ch = buf.spec().channels.count();
            let mut sb = SampleBuffer::<f32>::new(buf.capacity() as u64, *buf.spec());
            sb.copy_interleaved_ref(buf);
            out.extend_from_slice(sb.samples());
        }
        (rate, ch, out)
    }

    /// Corps d'un chunk RIFF (recherche simple, suffisante pour les tests).
    fn riff_chunk(data: &[u8], id: &[u8; 4]) -> Option<Vec<u8>> {
        let mut p = 12;
        while p + 8 <= data.len() {
            let size = u32::from_le_bytes(data[p + 4..p + 8].try_into().unwrap()) as usize;
            if &data[p..p + 4] == id {
                return Some(data[p + 8..p + 8 + size].to_vec());
            }
            p += 8 + size + (size & 1);
        }
        None
    }

    #[test]
    fn export_same_rate_is_exact_length() {
        let p = make_project("same", 48000, 30000);
        let files = export(&p, &settings(&p, 48000, BitDepth::I24));
        assert_eq!(files.len(), 2);
        let r = hound::WavReader::open(&files[0]).unwrap();
        assert_eq!(r.duration(), 30100);
        assert_eq!(r.spec().bits_per_sample, 24);
        let m = hound::WavReader::open(&files[1]).unwrap();
        assert_eq!(m.spec().channels, 2);
        // Réservation pour RF64 : chunk JUNK en tête.
        let bytes = std::fs::read(&files[0]).unwrap();
        assert_eq!(&bytes[12..16], b"JUNK");
        std::fs::remove_dir_all(&p.dir).ok();
    }

    #[test]
    fn aiff_size_limit_is_checked() {
        // 1 h stéréo 48 kHz 24 bits ≈ 1,04 Go : accepté.
        let hour = 3600 * 48000;
        assert_eq!(
            pcm_data_bytes(hour, 48000, 48000, 2, BitDepth::I24),
            hour * 2 * 3
        );
        assert!(check_aiff_size(hour, 48000, 48000, 2, BitDepth::I24).is_ok());
        // 4 h stéréo 48 kHz 32 bits ≈ 5,5 Go : refusé.
        let e = check_aiff_size(4 * hour, 48000, 48000, 2, BitDepth::I32).unwrap_err();
        assert!(e.to_string().contains("4 Go"), "{e}");
        // En 16 bits mono, ça passe.
        assert!(check_aiff_size(4 * hour, 48000, 48000, 1, BitDepth::I16).is_ok());
        // Rééchantillonnage vers 96 kHz : la taille double.
        assert_eq!(
            pcm_data_bytes(hour, 48000, 96000, 1, BitDepth::I16),
            hour * 2 * 2
        );
        // Juste à la limite.
        let max_frames = MAX_AIFF_DATA / 4;
        assert!(check_aiff_size(max_frames, 48000, 48000, 1, BitDepth::I32).is_ok());
        assert!(check_aiff_size(max_frames + 1, 48000, 48000, 1, BitDepth::I32).is_err());
    }

    #[test]
    fn export_resampled_length_and_level() {
        let p = make_project("rs", 48000, 48000);
        let files = export(&p, &settings(&p, 44100, BitDepth::I16));
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

    #[test]
    fn bwf_has_bext_and_data() {
        let p = make_project("bwf", 48000, 20000);
        let s = ExportSettings {
            format: ExportFormat::Bwf,
            mix: false,
            range: Some((1000, 15000)),
            ..settings(&p, 48000, BitDepth::F32)
        };
        let files = export(&p, &s);
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("01 - Kick.wav"));
        let bytes = std::fs::read(&files[0]).unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        let bext = riff_chunk(&bytes, b"bext").expect("chunk bext");
        assert!(bext.starts_with(b"Exp - Kick\0"));
        assert!(bext[256..].starts_with(b"TracksPlus\0"));
        assert_eq!(bext[288..320].iter().filter(|&&b| b != 0).count(), 32);
        let date = std::str::from_utf8(&bext[320..330]).unwrap();
        assert_eq!(date.as_bytes()[4], b'-');
        let tref = u64::from(u32::from_le_bytes(bext[338..342].try_into().unwrap()))
            | u64::from(u32::from_le_bytes(bext[342..346].try_into().unwrap())) << 32;
        assert_eq!(tref, 1000);
        assert_eq!(u16::from_le_bytes([bext[346], bext[347]]), 1);
        let history = std::str::from_utf8(&bext[602..]).unwrap();
        assert!(history.starts_with("A=PCM,F=48000,W=32,M=mono,T=TracksPlus"));
        let fact = riff_chunk(&bytes, b"fact").expect("chunk fact");
        assert_eq!(u32::from_le_bytes(fact[..4].try_into().unwrap()), 14000);
        // Lisible par un lecteur WAV standard, avec les bons échantillons.
        let mut r = hound::WavReader::open(&files[0]).unwrap();
        assert_eq!(r.spec().sample_format, hound::SampleFormat::Float);
        assert_eq!(r.duration(), 14000);
        let v: Vec<f32> = r.samples::<f32>().map(|x| x.unwrap()).collect();
        let t = 900.0 / 48000.0;
        let want = 0.5 * (2.0 * std::f32::consts::PI * 1000.0 * t as f32).sin();
        assert!((v[0] - want).abs() < 1e-6, "{} / {want}", v[0]);
        std::fs::remove_dir_all(&p.dir).ok();
    }

    #[test]
    fn aiff_decodes_back() {
        let p = make_project("aiff", 48000, 10000);
        let s = ExportSettings {
            format: ExportFormat::Aiff,
            // Le flottant devient du 32 bits entier en AIFF.
            ..settings(&p, 96000, BitDepth::F32)
        };
        let files = export(&p, &s);
        assert!(files[0].ends_with("01 - Kick.aif"));
        assert!(files[1].ends_with("Exp - Mix.aif"));
        let (rate, ch, v) = decode(&files[1]);
        assert_eq!((rate, ch), (96000, 2));
        assert_eq!(v.len() as u64, 2 * expected_frames(10100, 48000, 96000));
        // Amplitude conservée (léger dépassement du filtre à l'attaque brusque du sinus).
        let peak = v.iter().fold(0f32, |m, x| m.max(x.abs()));
        assert!((peak - 0.5).abs() < 0.05, "peak = {peak}");
        std::fs::remove_dir_all(&p.dir).ok();
    }

    #[test]
    fn flac_decodes_back() {
        let p = make_project("flac", 48000, 30000);
        let s = ExportSettings {
            format: ExportFormat::Flac,
            ..settings(&p, 48000, BitDepth::I24)
        };
        let files = export(&p, &s);
        assert!(files[0].ends_with("01 - Kick.flac"));
        let (rate, ch, v) = decode(&files[0]);
        assert_eq!((rate, ch), (48000, 1));
        assert_eq!(v.len(), 30100);
        // Sans perte : identique à l'export 24 bits, à la quantification près.
        let t = 1000.0 / 48000.0;
        let want = 0.5 * (2.0 * std::f32::consts::PI * 1000.0 * t as f32).sin();
        assert!((v[1100] - want).abs() < 1e-6, "{} / {want}", v[1100]);
        let (_, ch, v) = decode(&files[1]);
        assert_eq!(ch, 2);
        assert_eq!(v.len(), 2 * 30100);
        // Étiquettes Vorbis dans le fichier.
        let bytes = std::fs::read(&files[0]).unwrap();
        assert!(bytes.windows(10).any(|w| w == b"TITLE=Kick"));
        assert!(bytes.windows(9).any(|w| w == b"ALBUM=Exp"));
        std::fs::remove_dir_all(&p.dir).ok();

        // Dernière trame très courte (3 trames après les blocs de 4096), en 16 bits.
        let p = make_project("flac3", 48000, 2 * 4096 + 3 - 100);
        let s = ExportSettings {
            format: ExportFormat::Flac,
            mix: false,
            ..settings(&p, 48000, BitDepth::I16)
        };
        let files = export(&p, &s);
        let (_, _, v) = decode(&files[0]);
        assert_eq!(v.len(), 2 * 4096 + 3);
        std::fs::remove_dir_all(&p.dir).ok();
    }

    #[test]
    fn lossy_formats_produce_files() {
        let p = make_project("lossy", 48000, 48000);
        for (format, ext) in [(ExportFormat::Mp3, "mp3"), (ExportFormat::Ogg, "ogg")] {
            for mix_mono in [false, true] {
                let s = ExportSettings {
                    format,
                    mix_mono,
                    mp3_bitrate: if mix_mono {
                        Mp3Bitrate::V0
                    } else {
                        Mp3Bitrate::Cbr192
                    },
                    ogg_quality: if mix_mono {
                        OggQuality::Q5
                    } else {
                        OggQuality::Q9
                    },
                    // 96 kHz demandés : 48 kHz écrits (limite MP3/Ogg).
                    ..settings(&p, 96000, BitDepth::I24)
                };
                assert_eq!(s.output_rate(), 48000);
                assert_eq!(s.output_depth(), None);
                let files = export(&p, &s);
                assert_eq!(files.len(), 2);
                for f in &files {
                    assert_eq!(f.extension().unwrap(), ext);
                    assert!(std::fs::metadata(f).unwrap().len() > 1000);
                }
                let (rate, ch, v) = decode(&files[1]);
                assert_eq!(rate, 48000);
                assert_eq!(ch, if mix_mono { 1 } else { 2 });
                // Durée ≈ 1 s (le décodeur peut ajouter le délai de l'encodeur).
                let frames = v.len() / ch;
                assert!((47000..52000).contains(&frames), "{format:?} : {frames}");
                let peak = v.iter().fold(0f32, |m, x| m.max(x.abs()));
                assert!((0.4..0.65).contains(&peak), "{format:?} : peak = {peak}");
            }
        }
        // Famille 44,1 kHz : 88,2 kHz demandés → 44,1 kHz.
        assert_eq!(ExportFormat::Mp3.output_rate(88200), 44100);
        assert_eq!(ExportFormat::Ogg.output_rate(176400), 44100);
        assert_eq!(ExportFormat::Mp3.output_rate(44100), 44100);
        assert_eq!(ExportFormat::Flac.output_rate(192000), 192000);
        let bytes = std::fs::read(p.dir.join("Export").join("01 - Kick.mp3")).unwrap();
        assert_eq!(&bytes[..3], b"ID3");
        // Trame Info (CBR) réécrite en tête du flux à la fin de l'encodage.
        let head = &bytes[..bytes.len().min(8192)];
        assert!(head.windows(4).any(|w| w == b"Info" || w == b"Xing"));
        assert!(head.windows(4).any(|w| w == b"LAME"));
        assert!(head.windows(4).any(|w| w == b"Kick"));
        std::fs::remove_dir_all(&p.dir).ok();
    }

    #[test]
    fn mono_mix_is_mono() {
        let p = make_project("mono", 48000, 5000);
        let s = ExportSettings {
            stems: false,
            mix_mono: true,
            ..settings(&p, 48000, BitDepth::F32)
        };
        let files = export(&p, &s);
        let r = hound::WavReader::open(&files[0]).unwrap();
        assert_eq!(r.spec().channels, 1);
        assert_eq!(r.duration(), 5100);
        std::fs::remove_dir_all(&p.dir).ok();
    }

    #[test]
    fn peak_normalization_reaches_target() {
        let p = make_project("npk", 48000, 48000);
        let s = ExportSettings {
            normalize: Normalize::Peak(-3.0),
            ..settings(&p, 48000, BitDepth::F32)
        };
        let progress = AtomicU32::new(0);
        let files = run(&p, &s, &progress, &AtomicBool::new(false)).unwrap();
        assert_eq!(progress.load(Ordering::Relaxed), 1000);
        for f in &files {
            let mut r = hound::WavReader::open(f).unwrap();
            let peak = r
                .samples::<f32>()
                .map(|x| x.unwrap().abs())
                .fold(0f32, f32::max);
            let db = 20.0 * peak.log10();
            assert!((db + 3.0).abs() < 0.5, "{} : {db:.2} dBFS", f.display());
        }
        std::fs::remove_dir_all(&p.dir).ok();
    }

    #[test]
    fn lufs_normalization_reaches_target() {
        let p = make_project("nlu", 48000, 4 * 48000);
        for (format, depth) in [
            (ExportFormat::Wav, BitDepth::F32),
            (ExportFormat::Flac, BitDepth::I24),
        ] {
            let s = ExportSettings {
                format,
                normalize: Normalize::Lufs(-23.0),
                ..settings(&p, 44100, depth)
            };
            let files = export(&p, &s);
            for f in &files {
                let (rate, ch, v) = decode(f);
                let mut m = ebur128::EbuR128::new(ch as u32, rate, ebur128::Mode::I).unwrap();
                m.add_frames_f32(&v).unwrap();
                let l = m.loudness_global().unwrap();
                assert!((l + 23.0).abs() < 1.0, "{} : {l:.2} LUFS", f.display());
            }
        }
        // Cible trop forte : la crête vraie limite le gain à −1 dBTP.
        let g = lufs_gain(-20.0, 0.5, 0.0);
        let tp = 20.0 * (0.5 * g as f64).log10();
        assert!((tp + 1.0).abs() < 1e-3, "{tp}");
        // Silence : gain neutre.
        assert_eq!(lufs_gain(f64::NEG_INFINITY, 0.0, -14.0), 1.0);
        assert_eq!(peak_gain(0.0, -1.0), 1.0);
        std::fs::remove_dir_all(&p.dir).ok();
    }

    #[test]
    fn cancel_works_in_both_passes() {
        let p = make_project("cancel", 48000, 3 * CHUNK);
        for normalize in [Normalize::Off, Normalize::Lufs(-14.0)] {
            let s = ExportSettings {
                normalize,
                ..settings(&p, 48000, BitDepth::I24)
            };
            let e = run(&p, &s, &AtomicU32::new(0), &AtomicBool::new(true)).unwrap_err();
            assert!(e.to_string().contains("annulé"), "{e}");
            let left = std::fs::read_dir(&s.dir).unwrap().count();
            assert_eq!(left, 0, "fichier partiel laissé");
        }
        std::fs::remove_dir_all(&p.dir).ok();
    }

    #[test]
    fn format_depths_and_defaults() {
        assert_eq!(ExportFormat::default(), ExportFormat::Wav);
        assert_eq!(
            ExportFormat::Aiff.output_depth(BitDepth::F32),
            Some(BitDepth::I32)
        );
        assert_eq!(
            ExportFormat::Flac.output_depth(BitDepth::I32),
            Some(BitDepth::I24)
        );
        assert_eq!(
            ExportFormat::Wav.output_depth(BitDepth::F32),
            Some(BitDepth::F32)
        );
        assert!(ExportFormat::Mp3.depths().is_empty());
        // Anciens réglages enregistrés (sans les nouveaux champs) : toujours lisibles.
        let old = r#"{"sample_rate":48000,"depth":"I16","stems":true,"mix":false,
            "dither":true,"dir":"x"}"#;
        let s: ExportSettings = serde_json::from_str(old).unwrap();
        assert_eq!(s.format, ExportFormat::Wav);
        assert!(s.normalize.is_off());
        assert!(s.dither_applies());
        let json = serde_json::to_string(&ExportSettings {
            normalize: Normalize::Lufs(-14.0),
            ..Default::default()
        })
        .unwrap();
        let back: ExportSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.normalize, Normalize::Lufs(-14.0));
    }
}

#[cfg(test)]
mod range_tests {
    use super::*;
    use crate::project::Clip;
    use hound::{WavSpec, WavWriter};

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
            ..Default::default()
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
            ..Default::default()
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
