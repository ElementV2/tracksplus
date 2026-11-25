//! Export : pistes séparées (stems) et/ou mix (stéréo ou mono), avec conversion de
//! fréquence et de résolution, dans tous les formats courants : WAV, BWF (RF64 au-delà de
//! 4 Go), AIFF, FLAC, MP3 et Ogg Vorbis. Normalisation crête ou LUFS optionnelle, en
//! deux passes (mesure, puis export avec le gain calculé).

use crate::encode::{self, FileMeta, MAX_AIFF_DATA, SinkSpec};
use crate::project::{Project, sanitize};
use crate::render::{TrackRenderer, WavSource};
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

/// Export annulé par l'utilisateur (à distinguer d'une erreur).
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Export annulé")
    }
}

impl std::error::Error for Cancelled {}

/// Export réussi : fichiers écrits et avertissements à montrer à l'utilisateur.
pub struct Outcome {
    pub files: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

pub struct ExportJob {
    /// Avancement en millièmes (les deux passes de la normalisation comprises).
    pub progress: Arc<AtomicU32>,
    pub cancel: Arc<AtomicBool>,
    handle: Option<JoinHandle<Result<Outcome>>>,
}

impl ExportJob {
    pub fn start(project: &Project, settings: ExportSettings) -> Result<Self> {
        let progress = Arc::new(AtomicU32::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let (p2, c2) = (progress.clone(), cancel.clone());
        let project = project.clone();
        let handle = std::thread::Builder::new()
            .name("tracksplus-export".into())
            .spawn(move || {
                lower_thread_priority();
                run_export(&project, &settings, &p2, &c2)
            })
            .context("Impossible de démarrer l'export")?;
        Ok(Self {
            progress,
            cancel,
            handle: Some(handle),
        })
    }

    pub fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(|h| h.is_finished())
    }

    /// Attend la fin de l'export. Une erreur interne (panique) devient un message : les
    /// fichiers incomplets ont déjà été supprimés et les exports précédents sont intacts.
    pub fn join(mut self) -> Result<Outcome> {
        let Some(h) = self.handle.take() else {
            bail!("L'export a échoué");
        };
        match h.join() {
            Ok(r) => r,
            Err(p) => {
                let msg = p
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "erreur inconnue".into());
                bail!(
                    "L'export s'est arrêté sur une erreur interne ({msg}). Aucun fichier \
                     incomplet n'a été laissé et les exports précédents sont intacts."
                )
            }
        }
    }
}

/// L'export tourne sous la priorité normale : la lecture, l'enregistrement et les pads
/// restent prioritaires.
fn lower_thread_priority() {
    #[cfg(windows)]
    // SAFETY : appel sans pointeur sur le thread courant.
    unsafe {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
        };
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
}

/// Un fichier à produire.
struct Unit {
    /// Nom affiché dans les messages (« Mix », nom de la piste).
    name: String,
    path: PathBuf,
    /// Fichier en cours d'écriture, renommé en `path` une fois complet et sur le disque.
    part: PathBuf,
    /// Pistes mixées dans ce fichier (indices dans la liste des pistes exportées).
    tracks: Vec<usize>,
    channels: usize,
    meta: FileMeta,
}

/// Fichiers « .part » en cours d'écriture : supprimés s'ils n'ont pas été validés (erreur,
/// annulation, panique). Un export ne laisse ainsi jamais de fichier incomplet, et un
/// export précédent du même nom n'est remplacé qu'une fois le nouveau complet.
struct PartFiles(Vec<PathBuf>);

impl Drop for PartFiles {
    fn drop(&mut self) {
        for p in &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Nom de fichier lisible et de longueur raisonnable (limite de 255 caractères).
fn file_stem(name: &str) -> String {
    sanitize(name)
        .chars()
        .take(120)
        .collect::<String>()
        .trim()
        .to_string()
}

/// Dossier d'export : relatif à la session, jamais vide, jamais le dossier des prises.
fn export_dir(project: &Project, dir: &Path) -> Result<PathBuf> {
    if dir.as_os_str().is_empty() {
        bail!("Choisissez le dossier où écrire les fichiers exportés");
    }
    let dir = if dir.is_relative() {
        project.dir.join(dir)
    } else {
        dir.to_path_buf()
    };
    let canon = |p: &Path| {
        std::fs::canonicalize(p)
            .unwrap_or_else(|_| p.to_path_buf())
            .to_string_lossy()
            .to_lowercase()
    };
    if canon(&dir) == canon(&project.audio_dir()) {
        bail!(
            "Export impossible dans le dossier « Audio » de la session : il contient les prises \
             enregistrées. Choisissez un autre dossier, par exemple « Export »."
        );
    }
    Ok(dir)
}

/// Vérifie avant de commencer que chaque prise de la plage est présente et lisible en
/// entier : sinon l'export contiendrait des trous de silence.
fn check_takes(project: &Project, tracks: &[usize], start: u64, end: u64) -> Result<()> {
    let mut bad = Vec::new();
    for &i in tracks {
        for c in &project.tracks[i].clips {
            if c.end() <= start || c.start >= end {
                continue;
            }
            let path = project.clip_path(c);
            let why = if !path.exists() {
                Some("fichier introuvable".to_string())
            } else {
                match WavSource::open(&path) {
                    Err(e) => Some(format!("illisible ({e})")),
                    Ok(src) if src.frames() < c.offset + c.len => {
                        Some("fichier plus court que la prise (tronqué)".to_string())
                    }
                    Ok(_) => None,
                }
            };
            if let Some(why) = why {
                bad.push(format!("{} : {why}", c.file));
            }
        }
    }
    if bad.is_empty() {
        return Ok(());
    }
    bail!(
        "Export impossible : {} prise(s) manquante(s) ou illisible(s) dans le dossier « Audio » \
         de la session — {}",
        bad.len(),
        list(&bad)
    )
}

/// Liste courte pour un message : les 5 premiers éléments, puis le nombre restant.
fn list(items: &[String]) -> String {
    let mut s = items
        .iter()
        .take(5)
        .cloned()
        .collect::<Vec<_>>()
        .join(" · ");
    if items.len() > 5 {
        s += &format!(" · et {} autre(s)", items.len() - 5);
    }
    s
}

/// Message clair pour une erreur d'écriture (disque plein en particulier).
fn write_error(e: anyhow::Error, name: &str) -> anyhow::Error {
    let full = e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::StorageFull)
    });
    if full {
        anyhow!(
            "Disque plein pendant l'écriture de « {name} ». Libérez de la place puis relancez \
             l'export : les exports précédents sont intacts."
        )
    } else {
        e.context(format!("Écriture de « {name} »"))
    }
}

fn fmt_gb(bytes: f64) -> String {
    format!("{:.1} Go", bytes / 1e9).replace('.', ",")
}

/// Exporte la session selon `s`. Les fichiers sont écrits à côté de leur nom final
/// (« .part ») et ne prennent leur nom qu'une fois tous complets et sur le disque.
pub fn run_export(
    project: &Project,
    s: &ExportSettings,
    progress: &AtomicU32,
    cancel: &AtomicBool,
) -> Result<Outcome> {
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
    let dir = export_dir(project, &s.dir)?;

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
    let unit = |name: String, file: String, tracks: Vec<usize>, channels: usize| {
        let path = dir.join(format!("{file}.{ext}"));
        let part = dir.join(format!("{file}.{ext}.part"));
        Unit {
            meta: meta(name.clone()),
            name,
            path,
            part,
            tracks,
            channels,
        }
    };
    let mut units = Vec::new();
    if s.stems {
        for (k, &i) in with_audio.iter().enumerate() {
            let t = &project.tracks[i];
            // Piste stéréo : fichier stéréo ; piste mono : fichier mono.
            units.push(unit(
                t.name.clone(),
                format!("{:02} - {}", i + 1, file_stem(&t.name)),
                vec![k],
                t.channels(),
            ));
        }
    }
    if s.mix {
        units.push(unit(
            "Mix".into(),
            format!("{} - Mix", file_stem(&project.name)),
            (0..with_audio.len()).collect(),
            if s.mix_mono { 1 } else { 2 },
        ));
    }
    // Jamais un fichier de prise comme destination (dossier choisi à la main).
    for u in &units {
        let target = u.path.to_string_lossy().to_lowercase();
        let clash = project
            .tracks
            .iter()
            .flat_map(|t| &t.clips)
            .any(|c| project.clip_path(c).to_string_lossy().to_lowercase() == target);
        if clash {
            bail!(
                "Export impossible : « {} » remplacerait une prise de la session. Choisissez \
                 un autre dossier.",
                u.path.display()
            );
        }
    }

    // L'AIFF stocke ses tailles sur 32 bits : on refuse avant d'écrire plutôt que de
    // produire un fichier illisible. Le WAV/BWF passe en RF64 au-delà de 4 Go.
    if s.format == ExportFormat::Aiff {
        let widest = units.iter().map(|u| u.channels).max().unwrap_or(1);
        let depth = s.output_depth().unwrap_or(BitDepth::I32);
        check_aiff_size(len, sr_in, sr_out, widest, depth)?;
    }
    check_takes(project, &with_audio, start, end)?;

    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Création du dossier {}", dir.display()))?;
    // Place disponible : on refuse d'emblée plutôt que d'échouer au bout d'une heure.
    let secs = len as f64 / sr_in.max(1) as f64;
    let need: f64 = units
        .iter()
        .map(|u| s.estimated_bytes(secs, u.channels))
        .sum::<f64>()
        * 1.05
        + 20e6;
    if let Ok(free) = fs4::available_space(&dir)
        && need > free as f64
    {
        bail!(
            "Espace disque insuffisant : cet export demande environ {}, il reste {} dans {}. \
             Libérez de la place ou choisissez un autre dossier.",
            fmt_gb(need),
            fmt_gb(free as f64),
            dir.display()
        );
    }

    let passes = if s.normalize.is_off() { 1 } else { 2 };
    let mut pass = 0;
    let gains = match s.normalize {
        Normalize::Off => vec![1.0; units.len()],
        n => {
            let g = measure_all(
                project,
                &with_audio,
                &units,
                start,
                len,
                sr_out,
                n,
                &mut |f| {
                    let p = (pass as f64 + f.clamp(0.0, 1.0)) / passes as f64;
                    progress.store((p * 1000.0) as u32, Ordering::Relaxed);
                    !cancel.load(Ordering::Relaxed)
                },
            )?;
            pass += 1;
            g
        }
    };

    // Écriture : chaque fichier dans son « .part », supprimé en cas d'échec.
    let mut parts = PartFiles(Vec::new());
    let mut sinks = Vec::new();
    for (k, u) in units.iter().enumerate() {
        let _ = std::fs::remove_file(&u.part);
        parts.0.push(u.part.clone());
        let spec = SinkSpec {
            format: s.format,
            depth: s.output_depth().unwrap_or(BitDepth::F32),
            dither: s.dither,
            channels: u.channels,
            sample_rate: sr_out,
            mp3_bitrate: s.mp3_bitrate,
            ogg_quality: s.ogg_quality,
            meta: &u.meta,
            seed: k as u64 + 1,
        };
        sinks.push(encode::create(&u.part, &spec).map_err(|e| write_error(e, &u.name))?);
    }
    // En 32 bits flottant (WAV/BWF), rien n'est écrêté : au-delà de 0 dBFS, on prévient.
    let float_out = s.output_depth() == Some(BitDepth::F32)
        && matches!(s.format, ExportFormat::Wav | ExportFormat::Bwf);
    let mut over = vec![0u64; units.len()];
    let mut scaled = Vec::new();
    render_all(
        project,
        &with_audio,
        &units,
        start,
        len,
        sr_out,
        |k, b| {
            let g = gains[k];
            scaled.clear();
            scaled.extend(b.iter().map(|x| x * g));
            over[k] += scaled.iter().filter(|x| x.abs() > 1.0).count() as u64;
            sinks[k]
                .write(&scaled)
                .map_err(|e| write_error(e, &units[k].name))
        },
        &mut |f| {
            let p = (pass as f64 + f.clamp(0.0, 1.0)) / passes as f64;
            progress.store((p * 1000.0) as u32, Ordering::Relaxed);
            !cancel.load(Ordering::Relaxed)
        },
    )?;
    // Fin des fichiers : en-têtes, puis écriture forcée sur le disque.
    for (sink, u) in sinks.into_iter().zip(&units) {
        sink.finish().map_err(|e| write_error(e, &u.name))?;
    }
    // Tous complets : ils prennent leur nom (remplace un export précédent du même nom).
    let mut files = Vec::new();
    for u in &units {
        std::fs::rename(&u.part, &u.path).with_context(|| {
            format!(
                "Impossible de remplacer « {} » : le fichier est peut-être ouvert dans un autre \
                 programme. Fermez-le puis relancez l'export.",
                u.path.display()
            )
        })?;
        files.push(u.path.clone());
    }
    parts.0.clear();
    progress.store(1000, Ordering::Relaxed);

    let warnings = units
        .iter()
        .zip(&over)
        .filter(|(_, n)| **n > 0)
        .map(|(u, n)| {
            if float_out {
                format!(
                    "« {} » dépasse 0 dBFS ({n} échantillons) : rien n'est écrêté en 32 bits \
                     flottant, mais le son saturera à la lecture. Activez la normalisation \
                     (crête −1 dB) ou baissez le niveau.",
                    u.name
                )
            } else {
                format!(
                    "« {} » : {n} échantillons au-delà de 0 dBFS ont été écrêtés (saturation). \
                     Activez la normalisation (crête −1 dB) pour l'éviter.",
                    u.name
                )
            }
        })
        .collect();
    Ok(Outcome { files, warnings })
}

/// Pour les tests : l'export et la liste des fichiers.
#[cfg(test)]
fn run(
    project: &Project,
    s: &ExportSettings,
    progress: &AtomicU32,
    cancel: &AtomicBool,
) -> Result<Vec<PathBuf>> {
    run_export(project, s, progress, cancel).map(|o| o.files)
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
            "Export impossible : un fichier ferait {} Go, au-delà de la limite de 4 Go du \
             format AIFF. Choisissez le WAV ou le BWF (sans limite de taille), moins de bits, \
             une fréquence plus basse, des pistes séparées plutôt que le mix, ou une plage \
             plus courte.",
            format!("{:.1}", bytes as f64 / 1e9).replace('.', ",")
        );
    }
    Ok(())
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

/// Première passe de la normalisation : mesure chaque fichier et renvoie le gain linéaire
/// à appliquer à chacun.
#[allow(clippy::too_many_arguments)]
fn measure_all(
    project: &Project,
    tracks: &[usize],
    units: &[Unit],
    start: u64,
    len: u64,
    sr_out: u32,
    mode: Normalize,
    on_progress: &mut impl FnMut(f64) -> bool,
) -> Result<Vec<f32>> {
    use ebur128::{EbuR128, Mode};
    let sonie = |e: ebur128::Error| anyhow!("Mesure de sonie : {e:?}");
    let mut peaks = vec![0f32; units.len()];
    let mut meters = Vec::new();
    if let Normalize::Lufs(_) = mode {
        for u in units {
            meters.push(
                EbuR128::new(u.channels as u32, sr_out, Mode::I | Mode::TRUE_PEAK)
                    .map_err(sonie)?,
            );
        }
    }
    render_all(
        project,
        tracks,
        units,
        start,
        len,
        sr_out,
        |k, b| {
            match mode {
                Normalize::Lufs(_) => meters[k].add_frames_f32(b).map_err(sonie)?,
                _ => peaks[k] = b.iter().fold(peaks[k], |m, x| m.max(x.abs())),
            }
            Ok(())
        },
        on_progress,
    )?;
    Ok(match mode {
        Normalize::Off => vec![1.0; units.len()],
        Normalize::Peak(target) => peaks.iter().map(|&p| peak_gain(p, target)).collect(),
        Normalize::Lufs(target) => meters
            .iter()
            .zip(units)
            .map(|(m, u)| {
                let loudness = m.loudness_global().unwrap_or(f64::NEG_INFINITY);
                let true_peak = (0..u.channels as u32)
                    .filter_map(|c| m.true_peak(c).ok())
                    .fold(0f64, f64::max);
                lufs_gain(loudness, true_peak, target)
            })
            .collect(),
    })
}

/// Rend la plage `[start, start + len)` une seule fois pour tous les fichiers : chaque
/// piste est lue une fois par bloc, puis mixée dans les fichiers qui la contiennent (piste
/// séparée, mix). `sink(k, bloc)` reçoit les échantillons entrelacés du fichier `k`, déjà
/// à `sr_out` ; `on_progress(f)` renvoie `false` pour annuler. Une prise illisible en cours
/// de route arrête l'export avec son nom (jamais de trou de silence passé sous silence).
#[allow(clippy::too_many_arguments)]
fn render_all(
    project: &Project,
    tracks: &[usize],
    units: &[Unit],
    start: u64,
    len: u64,
    sr_out: u32,
    mut sink: impl FnMut(usize, &[f32]) -> Result<()>,
    on_progress: &mut impl FnMut(f64) -> bool,
) -> Result<()> {
    let sr_in = project.sample_rate;
    let mut renderers: Vec<TrackRenderer> = tracks
        .iter()
        .map(|&i| TrackRenderer::new(project, i))
        .collect();
    let mut bufs: Vec<Vec<f32>> = renderers
        .iter()
        .map(|r| vec![0f32; CHUNK * r.channels()])
        .collect();
    let mut chains = units
        .iter()
        .map(|u| Chain::new(u.channels, sr_in, sr_out, len))
        .collect::<Result<Vec<_>>>()?;
    let mut pos = 0u64;
    while pos < len {
        let n = ((len - pos) as usize).min(CHUNK);
        for (r, b) in renderers.iter_mut().zip(&mut bufs) {
            let b = &mut b[..n * r.channels()];
            r.render(start + pos, b);
            // Prise flottante abîmée : jamais de NaN ni d'infini dans un export.
            for x in b.iter_mut() {
                if !x.is_finite() {
                    *x = 0.0;
                }
            }
        }
        for (k, (u, c)) in units.iter().zip(&mut chains).enumerate() {
            c.begin(n);
            for &t in &u.tracks {
                let rc = renderers[t].channels();
                c.add(&bufs[t][..n * rc], rc);
            }
            c.process(&mut |b| sink(k, b))?;
        }
        pos += n as u64;
        let errors: Vec<String> = renderers.iter_mut().flat_map(|r| r.take_errors()).collect();
        if !errors.is_empty() {
            bail!(
                "Export interrompu : lecture impossible d'une prise — {}. Les exports \
                 précédents sont intacts.",
                list(&errors)
            );
        }
        if !on_progress(pos as f64 / len as f64) {
            return Err(Cancelled.into());
        }
    }
    for (k, c) in chains.iter_mut().enumerate() {
        c.flush(&mut |b| sink(k, b))?;
    }
    Ok(())
}

/// Chaîne de sortie d'un fichier : mixage de ses pistes, rééchantillonnage, puis découpe
/// exacte (latence du filtre, longueur attendue) en blocs entrelacés.
struct Chain {
    channels: usize,
    rs: Option<FftFixedIn<f32>>,
    /// Trames de latence du filtre encore à sauter.
    skip: usize,
    written: u64,
    expected: u64,
    input: Vec<Vec<f32>>,
    out: Vec<Vec<f32>>,
    inter: Vec<f32>,
}

impl Chain {
    fn new(channels: usize, sr_in: u32, sr_out: u32, len: u64) -> Result<Self> {
        let rs = if sr_in != sr_out {
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
        Ok(Self {
            channels,
            skip: rs.as_ref().map_or(0, |r| r.output_delay()),
            out: rs
                .as_ref()
                .map_or_else(Vec::new, |r| r.output_buffer_allocate(true)),
            rs,
            written: 0,
            expected: expected_frames(len, sr_in, sr_out),
            input: vec![Vec::with_capacity(CHUNK); channels],
            inter: Vec::new(),
        })
    }

    /// Prépare un bloc de `n` trames silencieuses.
    fn begin(&mut self, n: usize) {
        for ch in &mut self.input {
            ch.clear();
            ch.resize(n, 0.0);
        }
    }

    /// Ajoute une piste rendue (entrelacée sur `rc` canaux) au bloc.
    fn add(&mut self, buf: &[f32], rc: usize) {
        match (rc, self.channels) {
            // Mono : même signal sur chaque canal de sortie (au centre).
            (1, _) => {
                for ch in &mut self.input {
                    for (d, s) in ch.iter_mut().zip(buf) {
                        *d += *s;
                    }
                }
            }
            // Stéréo vers mono : moyenne des deux canaux.
            (_, 1) => {
                for (d, s) in self.input[0].iter_mut().zip(buf.as_chunks::<2>().0) {
                    *d += 0.5 * (s[0] + s[1]);
                }
            }
            _ => {
                for (j, ch) in self.input.iter_mut().enumerate() {
                    for (d, s) in ch.iter_mut().zip(buf.as_chunks::<2>().0) {
                        *d += s[j.min(1)];
                    }
                }
            }
        }
    }

    /// Rééchantillonne le bloc préparé et l'envoie.
    fn process(&mut self, sink: &mut impl FnMut(&[f32]) -> Result<()>) -> Result<()> {
        let n = self.input[0].len();
        let Self {
            rs,
            input,
            out,
            inter,
            skip,
            written,
            expected,
            ..
        } = self;
        match rs {
            None => emit(input, n, skip, written, *expected, inter, sink),
            Some(rs) => {
                let (_, got) = if n == CHUNK {
                    rs.process_into_buffer(input, out, None)?
                } else {
                    rs.process_partial_into_buffer(Some(input), out, None)?
                };
                emit(out, got, skip, written, *expected, inter, sink)
            }
        }
    }

    /// Vide le résampleur (latence du filtre) jusqu'à la longueur attendue.
    fn flush(&mut self, sink: &mut impl FnMut(&[f32]) -> Result<()>) -> Result<()> {
        let Self {
            rs,
            out,
            inter,
            skip,
            written,
            expected,
            ..
        } = self;
        if let Some(rs) = rs {
            let mut guard = 0;
            while *written < *expected && guard < 64 {
                let (_, got) = rs.process_partial_into_buffer::<Vec<f32>, _>(None, out, None)?;
                emit(out, got, skip, written, *expected, inter, sink)?;
                guard += 1;
            }
        }
        Ok(())
    }
}

/// Entrelace les `frames` premières trames de `block`, sans la latence restante ni
/// au-delà de la longueur attendue, et les envoie à `sink`.
fn emit(
    block: &[Vec<f32>],
    frames: usize,
    skip: &mut usize,
    written: &mut u64,
    expected: u64,
    inter: &mut Vec<f32>,
    sink: &mut impl FnMut(&[f32]) -> Result<()>,
) -> Result<()> {
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
    sink(inter)
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
    pub(super) fn decode(path: &Path) -> (u32, usize, Vec<f32>) {
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

/// Exports réels de plusieurs gigaoctets (lents : `cargo test --release -- --ignored
/// big_exports --nocapture`).
#[cfg(test)]
mod big_tests {
    use super::*;
    use crate::project::Clip;
    use hound::{WavSpec, WavWriter};
    use std::io::{Read, Seek, SeekFrom};

    const SR: u32 = 48000;
    /// 48 min 20 s : en 192 kHz flottant stéréo, le mix dépasse 4 Gio.
    const SECS: u64 = 2900;
    const FREQ: f32 = 441.0;

    fn long_project() -> Project {
        let dir = std::env::temp_dir().join(format!("tp-big-export-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut p = Project::new("Long".into(), dir, SR);
        p.ensure_dirs().unwrap();
        let id = p.add_track(Some("Voix".into()), Some(0));
        let spec = WavSpec {
            channels: 1,
            sample_rate: SR,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let frames = SECS * SR as u64;
        let mut w = WavWriter::create(p.audio_dir().join("long.wav"), spec).unwrap();
        let mut ph = 0f64;
        let step = 2.0 * std::f64::consts::PI * FREQ as f64 / SR as f64;
        for _ in 0..frames {
            w.write_sample(0.5 * ph.sin() as f32).unwrap();
            ph += step;
            if ph > 2.0 * std::f64::consts::PI {
                ph -= 2.0 * std::f64::consts::PI;
            }
        }
        w.finalize().unwrap();
        p.track_mut(id).unwrap().clips.push(Clip {
            id: 1,
            file: "long.wav".into(),
            start: 0,
            offset: 0,
            len: frames,
            src_len: frames,
            fade_in: 0,
            fade_out: 0,
        });
        p
    }

    fn u32le(b: &[u8], at: usize) -> u64 {
        u32::from_le_bytes(b[at..at + 4].try_into().unwrap()) as u64
    }

    fn u64le(b: &[u8], at: usize) -> u64 {
        u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
    }

    /// Vérifie un WAV/BWF exporté : en-tête RF64 cohérent avec la taille réelle, et un vrai
    /// signal (sinus à 0,5) dans les dernières secondes, au-delà de 4 Gio.
    fn check_rf64(path: &Path, bwf: bool, channels: u64, frames: u64) {
        let len = std::fs::metadata(path).unwrap().len();
        assert!(len > u32::MAX as u64, "{} : {len} octets", path.display());
        let mut f = std::fs::File::open(path).unwrap();
        let mut head = vec![0u8; 4096];
        f.read_exact(&mut head).unwrap();
        assert_eq!(&head[0..4], b"RF64");
        assert_eq!(u32le(&head, 4), u32::MAX as u64);
        assert_eq!(&head[8..12], b"WAVE");
        assert_eq!(&head[12..16], b"ds64");
        let ds = 20;
        let riff_size = u64le(&head, ds);
        let data_size = u64le(&head, ds + 8);
        let sample_count = u64le(&head, ds + 16);
        assert_eq!(riff_size, len - 8, "riffSize du ds64");
        // Parcours des chunks jusqu'à « data ».
        let mut p = 12usize;
        let mut data_at = None;
        let mut seen_bext = false;
        while p + 8 <= head.len() {
            let id = &head[p..p + 4];
            let size = u32le(&head, p + 4);
            if id == b"data" {
                assert_eq!(size, u32::MAX as u64, "taille 32 bits du data");
                data_at = Some(p as u64 + 8);
                break;
            }
            if id == b"bext" {
                seen_bext = true;
            }
            p += 8 + size as usize + (size as usize & 1);
        }
        let data_at = data_at.expect("chunk data introuvable");
        assert_eq!(seen_bext, bwf, "chunk bext");
        assert_eq!(
            data_at + data_size,
            len,
            "dataSize du ds64 = fin du fichier"
        );
        assert_eq!(data_size, frames * channels * 4);
        assert_eq!(sample_count, frames);
        // Dernières 2 s : signal attendu, aligné (flottants lisibles, amplitude 0,5).
        let tail = 2 * 192_000 * channels * 4;
        f.seek(SeekFrom::Start(len - tail)).unwrap();
        let mut buf = vec![0u8; tail as usize];
        f.read_exact(&mut buf).unwrap();
        let peak = buf
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes(*c).abs())
            .fold(0f32, f32::max);
        assert!(
            (0.45..=0.55).contains(&peak),
            "fin du fichier : crête {peak} (attendu ≈ 0,5)"
        );
        assert!(
            len - tail > u32::MAX as u64,
            "la vérification porte bien au-delà de 4 Gio"
        );
    }

    #[test]
    #[ignore]
    fn big_exports() {
        let t0 = std::time::Instant::now();
        let p = long_project();
        eprintln!("session de {SECS} s générée en {:.0?}", t0.elapsed());
        let frames_192 = SECS * 192_000;
        let base = ExportSettings {
            sample_rate: 192_000,
            depth: BitDepth::F32,
            stems: false,
            mix: true,
            dither: false,
            dir: p.dir.join("Export"),
            ..Default::default()
        };
        let go = |s: &ExportSettings| run(&p, s, &AtomicU32::new(0), &AtomicBool::new(false));

        // WAV et BWF : mix stéréo 192 kHz flottant ≈ 4,45 Go → RF64.
        for (fmt, bwf) in [(ExportFormat::Wav, false), (ExportFormat::Bwf, true)] {
            let t = std::time::Instant::now();
            let files = go(&ExportSettings {
                format: fmt,
                ..base.clone()
            })
            .unwrap();
            eprintln!("{fmt:?} : {:.1?}", t.elapsed());
            assert_eq!(files.len(), 1);
            check_rf64(&files[0], bwf, 2, frames_192);
            std::fs::remove_file(&files[0]).unwrap();
        }

        // AIFF : refusé avant d'écrire quoi que ce soit (limite de 4 Go du format).
        let err = go(&ExportSettings {
            format: ExportFormat::Aiff,
            depth: BitDepth::I32,
            ..base.clone()
        })
        .unwrap_err();
        eprintln!("AIFF : {err:#}");
        let left: Vec<_> = std::fs::read_dir(p.dir.join("Export"))
            .unwrap()
            .flatten()
            .collect();
        assert!(left.is_empty(), "aucun fichier laissé : {left:?}");

        // Formats compressés sur toute la durée : longueur exacte à la relecture.
        for (fmt, rate) in [
            (ExportFormat::Flac, 48_000u32),
            (ExportFormat::Mp3, 48_000),
            (ExportFormat::Ogg, 48_000),
        ] {
            let t = std::time::Instant::now();
            let files = go(&ExportSettings {
                format: fmt,
                sample_rate: rate,
                depth: BitDepth::I24,
                ..base.clone()
            })
            .unwrap();
            eprintln!("{fmt:?} : {:.1?}", t.elapsed());
            let (r, ch, samples) = super::tests::decode(&files[0]);
            assert_eq!(r, rate);
            assert_eq!(ch, 2);
            let got = (samples.len() / ch) as i64;
            let want = (SECS * rate as u64) as i64;
            // MP3 : quelques trames de délai d'encodeur tolérées. Ogg : le décodeur de test
            // ignore la coupure de fin ; la longueur exacte est vérifiée dans le fichier
            // (position « granule » de la dernière page).
            let tol = match fmt {
                ExportFormat::Mp3 => 4096,
                ExportFormat::Ogg => 2048,
                _ => 0,
            };
            if fmt == ExportFormat::Ogg {
                let bytes = std::fs::read(&files[0]).unwrap();
                let last = bytes
                    .windows(4)
                    .rposition(|w| w == b"OggS")
                    .expect("page Ogg");
                let granule = u64le(&bytes, last + 6);
                assert_eq!(granule, want as u64, "longueur Ogg dans le fichier");
            }
            assert!(
                (got - want).abs() <= tol,
                "{fmt:?} : {got} trames, attendu {want}"
            );
            let peak = samples[samples.len() - 2 * rate as usize * ch..]
                .iter()
                .fold(0f32, |m, x| m.max(x.abs()));
            assert!((0.4..=0.6).contains(&peak), "{fmt:?} : crête finale {peak}");
            std::fs::remove_file(&files[0]).unwrap();
        }
        let _ = std::fs::remove_dir_all(&p.dir);
        eprintln!("total : {:.0?}", t0.elapsed());
    }
}

/// Sûreté de l'export : rien n'est jamais perdu ni laissé à moitié écrit, et chaque
/// problème est signalé avec son détail.
#[cfg(test)]
mod safety_tests {
    use super::*;
    use crate::project::Clip;
    use hound::{WavSpec, WavWriter};

    fn project(tag: &str, level: f32, tracks: usize) -> Project {
        let dir = std::env::temp_dir().join(format!("tp-export-safe-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut p = Project::new("Safe".into(), dir, 48000);
        p.ensure_dirs().unwrap();
        let spec = WavSpec {
            channels: 1,
            sample_rate: 48000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        for k in 0..tracks {
            let file = format!("t{k}.wav");
            let mut w = WavWriter::create(p.audio_dir().join(&file), spec).unwrap();
            for _ in 0..48000 {
                w.write_sample(level).unwrap();
            }
            w.finalize().unwrap();
            let id = p.add_track(Some(format!("T{k}")), Some(k as u16));
            p.track_mut(id).unwrap().clips.push(Clip {
                id: 100 + k as u64,
                file,
                start: 0,
                offset: 0,
                len: 48000,
                src_len: 48000,
                fade_in: 0,
                fade_out: 0,
            });
        }
        p
    }

    fn settings(p: &Project) -> ExportSettings {
        ExportSettings {
            sample_rate: 48000,
            depth: BitDepth::I24,
            stems: false,
            mix: true,
            dir: p.dir.join("Export"),
            ..Default::default()
        }
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .map(|r| {
                r.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| n.ends_with(".part"))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Annuler (dans les deux passes) laisse l'export précédent intact, sans « .part ».
    #[test]
    fn cancel_keeps_previous_export() {
        let p = project("cancel", 0.25, 1);
        let s = settings(&p);
        let dir = p.dir.join("Export");
        let first = run(&p, &s, &AtomicU32::new(0), &AtomicBool::new(false)).unwrap();
        let before = std::fs::read(&first[0]).unwrap();
        for normalize in [Normalize::Off, Normalize::Peak(-1.0)] {
            let s = ExportSettings {
                normalize,
                ..s.clone()
            };
            let e = run_export(&p, &s, &AtomicU32::new(0), &AtomicBool::new(true))
                .err()
                .unwrap();
            assert!(e.is::<Cancelled>(), "{e:#}");
            assert_eq!(std::fs::read(&first[0]).unwrap(), before);
            assert!(leftovers(&dir).is_empty(), "{:?}", leftovers(&dir));
        }
        // Un nouvel export complet remplace bien l'ancien.
        let s = ExportSettings {
            depth: BitDepth::I16,
            ..s
        };
        let again = run(&p, &s, &AtomicU32::new(0), &AtomicBool::new(false)).unwrap();
        assert_eq!(again, first);
        assert_ne!(std::fs::read(&first[0]).unwrap(), before);
        assert!(leftovers(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&p.dir);
    }

    /// Une prise manquante ou tronquée : export refusé avant d'écrire, avec son nom.
    #[test]
    fn missing_or_truncated_take_is_reported() {
        let mut p = project("missing", 0.25, 2);
        std::fs::remove_file(p.audio_dir().join("t1.wav")).unwrap();
        let e = run(
            &p,
            &settings(&p),
            &AtomicU32::new(0),
            &AtomicBool::new(false),
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("t1.wav") && e.contains("introuvable"), "{e}");
        assert!(!p.dir.join("Export").join("Safe - Mix.wav").exists());
        // Prise plus longue que son fichier (fichier tronqué).
        p.tracks[1].clips.clear();
        p.tracks[0].clips[0].len = 96000;
        let e = run(
            &p,
            &settings(&p),
            &AtomicU32::new(0),
            &AtomicBool::new(false),
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("t0.wav") && e.contains("tronqué"), "{e}");
        let _ = std::fs::remove_dir_all(&p.dir);
    }

    /// Jamais d'export dans le dossier des prises, ni dans un dossier vide.
    #[test]
    fn audio_folder_and_empty_dir_are_refused() {
        let p = project("audio", 0.25, 1);
        for dir in [p.audio_dir(), PathBuf::from("Audio"), PathBuf::new()] {
            let s = ExportSettings {
                dir: dir.clone(),
                ..settings(&p)
            };
            let e = run(&p, &s, &AtomicU32::new(0), &AtomicBool::new(false));
            assert!(e.is_err(), "{}", dir.display());
        }
        // Dossier relatif : dans la session.
        let s = ExportSettings {
            dir: PathBuf::from("Rendus"),
            ..settings(&p)
        };
        let files = run(&p, &s, &AtomicU32::new(0), &AtomicBool::new(false)).unwrap();
        assert!(files[0].starts_with(p.dir.join("Rendus")));
        let _ = std::fs::remove_dir_all(&p.dir);
    }

    /// Un mix qui sature est signalé ; en 32 bits flottant, il n'est pas écrêté.
    #[test]
    fn clipping_is_reported() {
        let p = project("clip", 0.75, 2);
        let s = settings(&p);
        let out = run_export(&p, &s, &AtomicU32::new(0), &AtomicBool::new(false)).unwrap();
        assert_eq!(out.warnings.len(), 1, "{:?}", out.warnings);
        assert!(out.warnings[0].contains("écrêtés"));
        let s = ExportSettings {
            depth: BitDepth::F32,
            ..s
        };
        let out = run_export(&p, &s, &AtomicU32::new(0), &AtomicBool::new(false)).unwrap();
        assert!(out.warnings[0].contains("rien n'est écrêté"));
        let mut r = hound::WavReader::open(&out.files[0]).unwrap();
        let max = r
            .samples::<f32>()
            .map(|x| x.unwrap().abs())
            .fold(0f32, f32::max);
        assert!((max - 1.5).abs() < 1e-3, "{max}");
        // Avec la normalisation : plus de saturation, plus d'avertissement.
        let s = ExportSettings {
            normalize: Normalize::Peak(-1.0),
            ..settings(&p)
        };
        let out = run_export(&p, &s, &AtomicU32::new(0), &AtomicBool::new(false)).unwrap();
        assert!(out.warnings.is_empty(), "{:?}", out.warnings);
        let _ = std::fs::remove_dir_all(&p.dir);
    }

    /// Disque plein : message clair, qui nomme le fichier.
    #[test]
    fn disk_full_message() {
        let io = std::io::Error::new(std::io::ErrorKind::StorageFull, "plein");
        let e = write_error(anyhow::Error::new(io), "Mix").to_string();
        assert!(e.contains("Disque plein") && e.contains("Mix"), "{e}");
        let other = std::io::Error::other("autre");
        let e = format!("{:#}", write_error(anyhow::Error::new(other), "T1"));
        assert!(e.contains("T1") && e.contains("autre"), "{e}");
    }

    /// Pistes séparées et mix rendus en un seul passage : chaque fichier reste exact.
    #[test]
    fn single_pass_matches_each_file() {
        let p = project("pass", 0.25, 2);
        let s = ExportSettings {
            stems: true,
            mix: true,
            depth: BitDepth::F32,
            ..settings(&p)
        };
        let files = run(&p, &s, &AtomicU32::new(0), &AtomicBool::new(false)).unwrap();
        assert_eq!(files.len(), 3);
        let first = |f: &Path| {
            let mut r = hound::WavReader::open(f).unwrap();
            let ch = r.spec().channels;
            (
                ch,
                r.samples::<f32>().next().unwrap().unwrap(),
                r.duration(),
            )
        };
        assert_eq!(first(&files[0]), (1, 0.25, 48000));
        assert_eq!(first(&files[1]), (1, 0.25, 48000));
        assert_eq!(first(&files[2]), (2, 0.5, 48000));
        let _ = std::fs::remove_dir_all(&p.dir);
    }
}
