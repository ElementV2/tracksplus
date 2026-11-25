//! Lecture des prises depuis le disque (lecture / export) et calcul des formes d'onde.

use crate::project::{Clip, Project};
use hound::{SampleFormat, WavReader};
use parking_lot::RwLock;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

/// Tampon de lecture des fichiers WAV : large, pour lire chaque fichier par gros
/// morceaux (beaucoup de pistes sur un même disque = moins d'allers-retours).
const WAV_BUF: usize = 512 * 1024;

/// Nombre d'échantillons résumés par un point de forme d'onde (≈ 5 ms à 48 kHz : détail
/// fin au zoom ; au-delà, la courbe est lissée entre les points).
pub const PEAK_BLOCK: usize = 256;

/// Facteur entre les deux niveaux de détail de la forme d'onde.
pub const PEAK_L1: usize = 16;

/// Forme d'onde : (min, max) quantifiés sur i16, sur trois niveaux de détail
/// (`PEAK_BLOCK`, `× PEAK_L1` et `× PEAK_L1²` échantillons par point) : quel que soit le
/// zoom, un pixel ne lit qu'une poignée de points, même sur une prise de plusieurs heures.
/// Point de forme d'onde : (min, max) sur 8 bits, déjà à l'échelle d'affichage (racine
/// carrée de l'amplitude : les passages faibles gardent leur détail). Deux octets par point.
pub type Peak = [i8; 2];

pub struct PeakData {
    /// Identifiant unique (jamais réutilisé) : sert de clé au cache du dessin.
    pub id: u64,
    pub l0: Vec<Peak>,
    pub l1: Vec<Peak>,
    pub l2: Vec<Peak>,
}

impl Default for PeakData {
    fn default() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self {
            id: NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            l0: Vec::new(),
            l1: Vec::new(),
            l2: Vec::new(),
        }
    }
}

impl PeakData {
    /// Rend la mémoire réservée en trop une fois la forme d'onde complète.
    pub fn shrink(&mut self) {
        self.l0.shrink_to_fit();
        self.l1.shrink_to_fit();
        self.l2.shrink_to_fit();
    }

    pub fn extend(&mut self, new: &[Peak]) {
        for &p in new {
            self.l0.push(p);
            let i = self.l0.len() - 1;
            if i.is_multiple_of(PEAK_L1) {
                self.l1.push(p);
            } else if let Some(last) = self.l1.last_mut() {
                last[0] = last[0].min(p[0]);
                last[1] = last[1].max(p[1]);
            }
            if i.is_multiple_of(PEAK_L1 * PEAK_L1) {
                self.l2.push(p);
            } else if let Some(last) = self.l2.last_mut() {
                last[0] = last[0].min(p[0]);
                last[1] = last[1].max(p[1]);
            }
        }
    }
}

pub type Peaks = Arc<RwLock<PeakData>>;

pub fn to_peak(min: f32, max: f32) -> Peak {
    // Compression racine carrée (comme l'affichage) puis 8 bits.
    let q = |x: f32| {
        let x = if x.is_finite() {
            x.clamp(-1.0, 1.0)
        } else {
            0.0
        };
        (x.signum() * x.abs().sqrt() * 127.0).round() as i8
    };
    [q(min), q(max)]
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
    pub fn end_frame(&mut self, out: &mut Vec<Peak>) {
        self.n += 1;
        if self.n == PEAK_BLOCK {
            out.push(to_peak(self.min, self.max));
            self.n = 0;
            self.started = false;
        }
    }

    pub fn flush(&mut self, out: &mut Vec<Peak>) {
        if self.n > 0 {
            out.push(to_peak(self.min, self.max));
            self.n = 0;
            self.started = false;
        }
    }
}

/// Calcule la forme d'onde d'un fichier existant (thread de fond). Lue depuis le cache
/// `.peaks/` voisin s'il est à jour ; sinon calculée, puis mise en cache.
pub fn compute_peaks(path: &Path, dst: &Peaks) {
    let before = Stamp::of(path);
    if let Some(l0) = before.and_then(|st| load_cache(path, st)) {
        for c in l0.chunks(65536) {
            dst.write().extend(c);
        }
        return;
    }
    let Ok(mut src) = WavSource::open(path) else {
        return;
    };
    let ch = src.channels().min(2);
    let mut acc = PeakAccum::default();
    let mut buf = vec![0f32; 65536 * ch];
    let mut local = Vec::new();
    let mut all = Vec::new();
    loop {
        // Enregistrement en cours : le calcul cède le disque aux prises (lecture au ralenti,
        // ≈ 7 fois le temps réel à 48 kHz au lieu de la pleine vitesse du disque).
        if RECORDING.load(std::sync::atomic::Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
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
        all.extend_from_slice(&local);
        local.clear();
    }
    acc.flush(&mut local);
    dst.write().extend(&local);
    all.extend_from_slice(&local);
    drop(src);
    // Fichier modifié pendant le calcul : pas de cache (il serait faux).
    if let Some(st) = before
        && Stamp::of(path) == Some(st)
    {
        write_cache(path, st, &all);
        drop(all);
    }
    dst.write().shrink();
}

/// Enregistrement en cours (ou en finalisation) : les calculs de forme d'onde depuis le
/// disque ralentissent pour ne pas disputer le disque aux prises.
static RECORDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_recording(on: bool) {
    RECORDING.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Travaux de forme d'onde faits hors du thread de l'interface.
enum PeakTask {
    /// Calcul depuis le fichier (ou son cache).
    Compute(PathBuf, Peaks),
    /// Forme d'onde complète d'un fichier fermé : mémoire rendue, puis mise en cache.
    Store(PathBuf, Peaks),
    /// Fichier d'une prise longue : sa part de la forme d'onde en direct (points `a..b` du
    /// niveau fin), sans relire le fichier, puis mise en cache.
    Slice {
        src: Peaks,
        a: usize,
        b: usize,
        path: PathBuf,
        dst: Peaks,
    },
}

/// Petit groupe de threads partagé (deux fichiers à la fois au plus : le disque n'est pas
/// saturé), en priorité basse, entrées-sorties comprises : jamais devant les prises.
fn peak_pool() -> &'static crossbeam_channel::Sender<PeakTask> {
    static POOL: OnceLock<crossbeam_channel::Sender<PeakTask>> = OnceLock::new();
    POOL.get_or_init(|| {
        let (tx, rx) = crossbeam_channel::unbounded::<PeakTask>();
        for i in 0..2 {
            let rx = rx.clone();
            let _ = std::thread::Builder::new()
                .name(format!("tracksplus-peaks-{i}"))
                .spawn(move || {
                    lower_thread_priority();
                    while let Ok(task) = rx.recv() {
                        // Un fichier piégé ne doit pas arrêter le calcul des suivants.
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            run_task(task)
                        }));
                    }
                });
        }
        tx
    })
}

fn run_task(task: PeakTask) {
    match task {
        PeakTask::Compute(path, dst) => {
            // Session refermée entre-temps : plus personne n'attend ce calcul.
            if Arc::strong_count(&dst) > 1 {
                compute_peaks(&path, &dst);
            }
        }
        PeakTask::Store(path, peaks) => {
            peaks.write().shrink();
            store_peaks(&path, &peaks.read());
        }
        PeakTask::Slice {
            src,
            a,
            b,
            path,
            dst,
        } => {
            let part: Vec<Peak> = {
                let s = src.read();
                let b = b.min(s.l0.len());
                s.l0[a.min(b)..b].to_vec()
            };
            drop(src);
            {
                let mut d = dst.write();
                d.extend(&part);
                d.shrink();
            }
            store_peaks(&path, &dst.read());
        }
    }
}

/// Threads de forme d'onde : priorité « arrière-plan » (Windows : processeur et disque).
fn lower_thread_priority() {
    #[cfg(windows)]
    // SAFETY : appel sans pointeur sur le thread courant.
    unsafe {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN,
        };
        SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN);
    }
}

/// Demande le calcul de la forme d'onde de `path` dans `dst` (thread de fond).
pub fn request_peaks(path: PathBuf, dst: Peaks) {
    let _ = peak_pool().send(PeakTask::Compute(path, dst));
}

/// Fichier de prise fermé dont la forme d'onde complète est déjà en mémoire : réduction
/// de la mémoire réservée et mise en cache faites en arrière-plan (pas de gel au Stop).
pub fn store_peaks_later(path: PathBuf, peaks: Peaks) {
    let _ = peak_pool().send(PeakTask::Store(path, peaks));
}

/// Forme d'onde d'un fichier d'une prise longue, découpée dans celle calculée en direct
/// pour toute la prise : points `a..b` du niveau fin (`dst` se remplit en arrière-plan).
pub fn slice_peaks_later(src: Peaks, a: usize, b: usize, path: PathBuf, dst: Peaks) {
    let _ = peak_pool().send(PeakTask::Slice {
        src,
        a,
        b,
        path,
        dst,
    });
}

/// Met en cache la forme d'onde d'un fichier terminé (par exemple calculée en direct
/// pendant l'enregistrement). À appeler une fois le fichier WAV fermé. Erreurs ignorées.
pub fn store_peaks(path: &Path, data: &PeakData) {
    if let Some(st) = Stamp::of(path) {
        write_cache(path, st, &data.l0);
    }
}

/// Identité d'un fichier source : taille et date de modification.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Stamp {
    len: u64,
    secs: u64,
    nanos: u32,
}

impl Stamp {
    fn of(path: &Path) -> Option<Self> {
        let m = std::fs::metadata(path).ok()?;
        let t = m
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?;
        Some(Self {
            len: m.len(),
            secs: t.as_secs(),
            nanos: t.subsec_nanos(),
        })
    }
}

const CACHE_MAGIC: &[u8; 4] = b"TPPK";
const CACHE_VERSION: u32 = 2;
/// magic, version, PEAK_BLOCK, taille, secondes, nanosecondes, nombre de points.
const CACHE_HEADER: usize = 4 + 4 + 4 + 8 + 8 + 4 + 8;

/// `<dossier>/.peaks/<fichier>.peaks`
fn cache_path(path: &Path) -> Option<PathBuf> {
    let mut name = path.file_name()?.to_os_string();
    name.push(".peaks");
    Some(path.parent()?.join(".peaks").join(name))
}

/// Points (niveau fin) du cache, s'il correspond exactement au fichier source.
fn load_cache(path: &Path, st: Stamp) -> Option<Vec<Peak>> {
    let b = std::fs::read(cache_path(path)?).ok()?;
    let h = b.get(..CACHE_HEADER)?;
    let u32_at = |o: usize| u32::from_le_bytes(h[o..o + 4].try_into().unwrap());
    let u64_at = |o: usize| u64::from_le_bytes(h[o..o + 8].try_into().unwrap());
    if &h[..4] != CACHE_MAGIC
        || u32_at(4) != CACHE_VERSION
        || u32_at(8) as usize != PEAK_BLOCK
        || u64_at(12) != st.len
        || u64_at(20) != st.secs
        || u32_at(28) != st.nanos
    {
        return None;
    }
    let n = usize::try_from(u64_at(32)).ok()?;
    let body = &b[CACHE_HEADER..];
    if body.len() != n.checked_mul(2)? {
        return None;
    }
    Some(
        body.as_chunks::<2>()
            .0
            .iter()
            .map(|c| [c[0] as i8, c[1] as i8])
            .collect(),
    )
}

/// Écrit le cache (fichier temporaire puis renommage : jamais de cache à moitié écrit).
fn write_cache(path: &Path, st: Stamp, l0: &[Peak]) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let Some(dst) = cache_path(path) else {
        return;
    };
    let Some(dir) = dst.parent() else {
        return;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let mut b = Vec::with_capacity(CACHE_HEADER + l0.len() * 2);
    b.extend_from_slice(CACHE_MAGIC);
    b.extend_from_slice(&CACHE_VERSION.to_le_bytes());
    b.extend_from_slice(&(PEAK_BLOCK as u32).to_le_bytes());
    b.extend_from_slice(&st.len.to_le_bytes());
    b.extend_from_slice(&st.secs.to_le_bytes());
    b.extend_from_slice(&st.nanos.to_le_bytes());
    b.extend_from_slice(&(l0.len() as u64).to_le_bytes());
    b.extend(l0.iter().flat_map(|p| [p[0] as u8, p[1] as u8]));
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut tmp_name = dst.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(format!(".{}-{seq}.tmp", std::process::id()));
    let tmp = dir.join(tmp_name);
    if std::fs::write(&tmp, &b).is_err() || std::fs::rename(&tmp, &dst).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
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
    /// Échantillons illisibles remplacés par du silence (fichier abîmé).
    read_errors: u64,
    /// Repositionnement impossible : la suite est lue comme du silence.
    seek_failed: bool,
}

impl WavSource {
    pub fn open(path: &Path) -> hound::Result<Self> {
        let file = File::open(path)?;
        let reader = WavReader::new(BufReader::with_capacity(WAV_BUF, file))?;
        let spec = reader.spec();
        // En-tête annonçant plus de données que le fichier n'en contient (fichier tronqué
        // ou abîmé) : on s'en tient à ce qui existe vraiment, sinon des milliards
        // d'échantillons « illisibles » seraient lus un par un.
        let file_len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(u64::MAX);
        let frame_bytes =
            (spec.channels.max(1) as u64) * (spec.bits_per_sample.div_ceil(8).max(1) as u64);
        let frames = (reader.duration() as u64).min(file_len / frame_bytes);
        Ok(Self {
            float: spec.sample_format == SampleFormat::Float,
            // En-tête abîmé : nombre de bits borné (pas de décalage hors limites).
            scale: 1.0 / (1u64 << (spec.bits_per_sample.clamp(1, 32) - 1)) as f32,
            channels: spec.channels.max(1) as usize,
            reader,
            pos: 0,
            frames,
            read_errors: 0,
            seek_failed: false,
        })
    }

    /// Longueur du fichier (trames).
    pub fn frames(&self) -> u64 {
        self.frames
    }

    pub fn seek(&mut self, frame: u64) {
        let frame = frame.min(self.frames);
        if frame != self.pos {
            if self.reader.seek(frame as u32).is_ok() {
                self.pos = frame;
            } else {
                // Échec de lecture : silence plutôt qu'un mauvais passage audio.
                self.pos = self.frames;
                self.seek_failed = true;
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
        let scale = self.scale;
        // Échantillons du fichier, convertis en f32 (une erreur de lecture = silence,
        // comptée pour être signalée).
        let errs = std::cell::Cell::new(0u64);
        let n = if self.float {
            fill(
                self.reader
                    .samples::<f32>()
                    .take(total)
                    .map_while(|s| match s {
                        // NaN / infini dans un fichier flottant abîmé : silence (ils
                        // contamineraient mixage, vumètres et formes d'onde).
                        // Bornés à +24 dB : la somme de deux canaux ne peut pas déborder.
                        Ok(v) if v.is_finite() => Some(v.clamp(-16.0, 16.0)),
                        Ok(_) => Some(0.0),
                        // Erreur de lecture (fin de fichier prématurée) : on s'arrête là.
                        Err(_) => {
                            errs.set(errs.get() + 1);
                            None
                        }
                    }),
                fc,
                out_ch,
                out,
            )
        } else {
            fill(
                self.reader
                    .samples::<i32>()
                    .take(total)
                    .map_while(|s| match s {
                        Ok(v) => Some(v as f32 * scale),
                        Err(_) => {
                            errs.set(errs.get() + 1);
                            None
                        }
                    }),
                fc,
                out_ch,
                out,
            )
        };
        self.read_errors += errs.get();
        self.pos += n as u64;
        if errs.get() > 0 {
            // Plus rien de lisible au-delà : la suite est du silence, sans relire.
            self.frames = self.pos;
        }
        n
    }
}

/// Répartit les échantillons d'un fichier de `fc` canaux sur `out_ch` canaux (1 ou 2) :
/// mono → stéréo par duplication, stéréo → mono par moyenne des deux premiers canaux.
/// Retourne le nombre de trames écrites.
fn fill(samples: impl Iterator<Item = f32>, fc: usize, out_ch: usize, out: &mut [f32]) -> usize {
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
    n
}

struct ClipSource {
    clip: Clip,
    path: PathBuf,
    src: Option<WavSource>,
    failed: bool,
    /// Problème déjà signalé (une seule fois par prise).
    reported: bool,
}

/// Rend le signal d'une piste (mono, ou stéréo entrelacé) à n'importe quelle position
/// de la timeline.
pub struct TrackRenderer {
    clips: Vec<ClipSource>,
    tmp: Vec<f32>,
    channels: usize,
    errors: Vec<String>,
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
                reported: false,
            })
            .collect();
        Self {
            clips,
            tmp: Vec::new(),
            channels: track.channels(),
            errors: Vec::new(),
        }
    }

    /// Prises qui n'ont pas pu être lues correctement depuis le dernier appel (fichier
    /// absent, illisible ou abîmé) : remplacées par du silence à l'écoute, signalées à
    /// l'export.
    pub fn take_errors(&mut self) -> Vec<String> {
        std::mem::take(&mut self.errors)
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
                    Err(e) => {
                        cs.failed = true;
                        cs.reported = true;
                        self.errors.push(format!("{} : {e}", c.file));
                    }
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
                    Err(e) => {
                        cs.failed = true;
                        if !cs.reported {
                            cs.reported = true;
                            self.errors.push(format!("{} : {e}", c.file));
                        }
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
            if !cs.reported && (src.seek_failed || src.read_errors > 0 || got < n) {
                cs.reported = true;
                self.errors
                    .push(format!("{} : fichier abîmé ou tronqué", c.file));
            }
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

    /// Fichier d'une prise longue : sa forme d'onde est découpée dans celle de la prise
    /// entière (sans relecture), réduite, et mise en cache à côté du fichier.
    #[test]
    fn slice_task_cuts_live_peaks_and_caches_them() {
        let dir = std::env::temp_dir().join(format!("tp-slice-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t_p2.wav");
        std::fs::write(&path, b"RIFF").unwrap();
        let src: Peaks = Arc::new(RwLock::new(PeakData::default()));
        let pts: Vec<Peak> = (0..100).map(|i| [-(i as i8), i as i8]).collect();
        src.write().extend(&pts);
        let dst: Peaks = Arc::new(RwLock::new(PeakData::default()));
        run_task(PeakTask::Slice {
            src: src.clone(),
            a: 40,
            b: 1000,
            path: path.clone(),
            dst: dst.clone(),
        });
        assert_eq!(dst.read().l0, pts[40..]);
        assert_eq!(dst.read().l0.capacity(), 60);
        let st = Stamp::of(&path).unwrap();
        assert_eq!(load_cache(&path, st).unwrap(), pts[40..]);
        std::fs::remove_dir_all(dir).ok();
    }

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

    fn write_wav(path: &Path, frames: usize, v: f32) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for i in 0..frames {
            w.write_sample(if i % 2 == 0 { v } else { -v }).unwrap();
        }
        w.finalize().unwrap();
    }

    fn peaks_of(path: &Path) -> PeakData {
        let p: Peaks = Arc::new(RwLock::new(PeakData::default()));
        compute_peaks(path, &p);
        std::mem::take(&mut *p.write())
    }

    /// Cache des formes d'onde : écrit au premier calcul, relu ensuite, ignoré dès que
    /// le fichier source change.
    #[test]
    fn peaks_cache_roundtrip_and_invalidation() {
        let dir = std::env::temp_dir().join(format!("tp-peaks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let audio = dir.join("Audio");
        std::fs::create_dir_all(&audio).unwrap();
        let wav = audio.join("x.wav");
        write_wav(&wav, 5000, 0.5);
        let a = peaks_of(&wav);
        assert_eq!(a.l0.len(), 5000usize.div_ceil(PEAK_BLOCK));
        let cache = audio.join(".peaks").join("x.wav.peaks");
        assert!(cache.exists(), "cache écrit");
        let st = Stamp::of(&wav).unwrap();
        assert_eq!(load_cache(&wav, st).unwrap(), a.l0);

        // Le cache est bien utilisé : des données posées par store_peaks sont relues.
        let mut fake = PeakData::default();
        fake.extend(&[[-7, 7]; 20]);
        store_peaks(&wav, &fake);
        let b = peaks_of(&wav);
        assert_eq!(b.l0, fake.l0);
        assert_eq!(b.l1, fake.l1, "niveau grossier reconstruit");

        // Fichier modifié : cache rejeté, forme d'onde recalculée puis remise en cache.
        write_wav(&wav, 9000, 0.25);
        assert!(load_cache(&wav, Stamp::of(&wav).unwrap()).is_none());
        let c = peaks_of(&wav);
        let n9 = 9000usize.div_ceil(PEAK_BLOCK);
        assert_eq!(c.l0.len(), n9);
        assert_eq!(c.l0[0], to_peak(-0.25, 0.25));
        assert_eq!(load_cache(&wav, Stamp::of(&wav).unwrap()).unwrap(), c.l0);

        // Cache corrompu : ignoré.
        std::fs::write(&cache, b"TPPK").unwrap();
        assert_eq!(peaks_of(&wav).l0, c.l0);

        // Groupe de threads : le résultat finit par arriver.
        let p: Peaks = Arc::new(RwLock::new(PeakData::default()));
        request_peaks(wav.clone(), p.clone());
        let t = std::time::Instant::now();
        while p.read().l0.len() < n9 && t.elapsed().as_secs() < 10 {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(p.read().l0, c.l0);
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
