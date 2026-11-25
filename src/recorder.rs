//! Thread disque : vide les ring buffers d'entrée dans des fichiers WAV (mono ou stéréo).
//!
//! Sécurité « live » :
//! - en-têtes WAV mis à jour toutes les 2 s et données forcées sur le disque (fsync)
//!   toutes les 10 s : après un crash, les fichiers sont lisibles (voir `recovery`) ;
//! - au-delà de ~2 Go un fichier est clos et la prise continue dans `nom_p2.wav`,
//!   `nom_p3.wav`… (un WAV ne peut pas dépasser 4 Go) ;
//! - à l'arrêt, tout ce qui reste dans les tampons est écrit avant de fermer ;
//! - toute erreur disque est remontée immédiatement à l'interface.

use crate::project::RecFormat;
use crate::render::{PeakAccum, Peaks};
use crossbeam_channel::Sender;
use hound::WavSpec;
use rtrb::{Consumer, Producer, RingBuffer};
use std::fs::File;
use std::io::BufWriter;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Taille maximale d'un fichier avant passage au suivant (marge sous la limite 32 bits,
/// et compatible avec les logiciels qui lisent la taille en entier signé).
pub const MAX_SEGMENT_BYTES: u64 = 2_000_000_000;
const FLUSH_EVERY: Duration = Duration::from_secs(2);
const SYNC_EVERY: Duration = Duration::from_secs(10);
/// À l'arrêt, abandon seulement si plus rien n'avance pendant ce délai.
/// Délai entre deux essais de réouverture d'un fichier après une erreur d'écriture.
const RETRY_EVERY: Duration = Duration::from_secs(1);
const STALL_TIMEOUT: Duration = Duration::from_secs(3);
const WRITE_BUFFER: usize = 256 * 1024;

pub struct RecSpec {
    pub track_id: u64,
    pub file: String,
    pub path: PathBuf,
    pub peaks: Peaks,
    /// Échantillons entrelacés de `channels` canaux (1 = mono, 2 = stéréo).
    pub cons: Consumer<f32>,
    pub channels: u16,
}

/// Un fichier d'une prise (une prise longue peut en compter plusieurs).
#[derive(Debug, Clone)]
pub struct Segment {
    pub file: String,
    pub frames: u64,
    /// Position du début du fichier dans la prise (trames depuis le début de la prise).
    /// Égale à la somme des fichiers précédents, sauf après une erreur d'écriture : les
    /// trames perdues entre deux fichiers sont un trou, pas un décalage.
    pub start: u64,
}

#[derive(Debug)]
pub struct RecResult {
    pub track_id: u64,
    pub segments: Vec<Segment>,
    pub error: Option<String>,
}

impl RecResult {
    /// Durée couverte par la prise (fin du dernier fichier, trous compris).
    pub fn frames(&self) -> u64 {
        self.segments
            .iter()
            .map(|s| s.start + s.frames)
            .max()
            .unwrap_or(0)
    }

    /// Trames réellement écrites sur le disque.
    #[cfg(test)]
    pub fn written(&self) -> u64 {
        self.segments.iter().map(|s| s.frames).sum()
    }
}

/// Fichier « <nom>.start » : position (trames) d'un fichier repris après une erreur
/// d'écriture, pour que la récupération après un crash le pose à sa vraie place.
pub fn start_sidecar(file: &str) -> String {
    format!("{file}.start")
}

/// Threads disque (écriture des prises, lecture des pistes) : priorité haute, juste
/// sous celle des callbacks audio, comme dans les logiciels pro — l'interface, l'export
/// ou l'antivirus ne doivent jamais les retarder au point de vider leurs tampons.
pub fn raise_disk_thread_priority() {
    #[cfg(windows)]
    // SAFETY : appel sans pointeur sur le thread courant.
    unsafe {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
        };
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
    }
}

/// Nom du k-ième fichier d'une prise (k = 1 : nom de base).
pub fn segment_name(base: &str, k: usize) -> String {
    if k <= 1 {
        base.to_string()
    } else {
        format!("{}_p{k}.wav", base.trim_end_matches(".wav"))
    }
}

type Errors = Arc<parking_lot::Mutex<Vec<String>>>;

pub struct Recorder {
    stop: Arc<AtomicBool>,
    /// Erreurs disque survenues pendant la prise (affichées en direct).
    errors: Errors,
    handle: Option<JoinHandle<Vec<RecResult>>>,
    /// Copie de sécurité sur un second disque (son propre thread et ses propres tampons).
    backup: Option<Box<Recorder>>,
    /// Échantillons que la copie n'a pas pu recevoir (second disque trop lent).
    copy_lost: Arc<AtomicU64>,
}

struct SegWriter {
    w: TakeWav,
    /// Second accès au fichier pour forcer l'écriture sur le disque (partagé avec le
    /// thread de synchronisation : jamais plus d'une demande en attente par fichier).
    file: Arc<File>,
    frames: u64,
    /// Position du début de ce fichier dans la prise.
    start: u64,
}

/// Consommateur séparé de l'état d'écriture (emprunts disjoints dans la boucle).
struct Lane {
    cons: Consumer<f32>,
    st: LaneState,
    /// Copie de sécurité (second disque) : mêmes échantillons, vers un autre thread.
    copy: Option<CopyOut>,
}

/// Envoi des échantillons vers l'enregistreur de la copie de sécurité. Jamais bloquant :
/// si le second disque ne suit pas, la copie perd des échantillons (remplacés par du
/// silence, elle reste calée), l'enregistrement principal n'est jamais ralenti.
struct CopyOut {
    prod: Producer<f32>,
    /// Échantillons perdus, à remplacer par du silence dès qu'il y a de la place.
    pending: usize,
    lost: Arc<AtomicU64>,
}

impl CopyOut {
    fn forward(&mut self, a: &[f32], b: &[f32]) {
        if self.prod.is_abandoned() {
            return;
        }
        if self.pending > 0 {
            let z = self.pending.min(self.prod.slots());
            if let Ok(c) = self.prod.write_chunk_uninit(z) {
                c.fill_from_iter(std::iter::repeat_n(0.0f32, z));
            }
            self.pending -= z;
        }
        let total = a.len() + b.len();
        let n = if self.pending > 0 {
            0
        } else {
            total.min(self.prod.slots())
        };
        if n > 0
            && let Ok(c) = self.prod.write_chunk_uninit(n)
        {
            c.fill_from_iter(a.iter().chain(b).copied().take(n));
        }
        if n < total {
            self.pending += total - n;
            self.lost.fetch_add((total - n) as u64, Ordering::Relaxed);
        }
    }
}

struct LaneState {
    track_id: u64,
    file: String,
    peaks: Peaks,
    dir: PathBuf,
    wav: WavSpec,
    /// Canal suivant à écrire dans la trame en cours.
    k: usize,
    cur: Option<SegWriter>,
    done: Vec<Segment>,
    acc: PeakAccum,
    error: Option<String>,
    /// Trames complètes écoulées depuis le début de la prise (écrites ou perdues) : position
    /// du prochain fichier ouvert, pour que la piste reste calée après une erreur.
    elapsed: u64,
    /// Prochain essai de réouverture après une erreur d'écriture.
    retry_at: Option<Instant>,
    /// Date et heure du début de la prise, et sa position depuis minuit (trames) :
    /// TimeReference du BWF.
    take_time: chrono::NaiveDateTime,
    time_ref: u64,
    /// Copie de sécurité : un fichier homonyme existant est mis de côté.
    move_aside: bool,
}

/// Fichier WAV d'une prise, au format BWF : en-tête `fmt`, bloc `bext` (date, heure et
/// position depuis minuit — les logiciels pro s'en servent pour poser le fichier à son
/// heure réelle ou caler plusieurs enregistreurs), puis les données. Les tailles de
/// l'en-tête sont réécrites à chaque `flush` : le fichier reste lisible après un crash.
struct TakeWav {
    w: BufWriter<File>,
    /// Position du champ « taille » du bloc `data`.
    data_size_at: u64,
    bytes: u64,
    /// Octet de bourrage final écrit (`finalize`).
    padded: bool,
}

impl TakeWav {
    fn create(file: File, spec: WavSpec, bext: &[u8]) -> std::io::Result<Self> {
        let float = spec.sample_format == hound::SampleFormat::Float;
        debug_assert!(float == (spec.bits_per_sample == 32));
        let bytes_per_sample = spec.bits_per_sample / 8;
        let block = spec.channels * bytes_per_sample;
        let mut h = Vec::with_capacity(64 + bext.len());
        h.extend_from_slice(b"RIFF\0\0\0\0WAVEfmt ");
        h.extend_from_slice(&16u32.to_le_bytes());
        h.extend_from_slice(&(if float { 3u16 } else { 1u16 }).to_le_bytes());
        h.extend_from_slice(&spec.channels.to_le_bytes());
        h.extend_from_slice(&spec.sample_rate.to_le_bytes());
        h.extend_from_slice(&(spec.sample_rate * block as u32).to_le_bytes());
        h.extend_from_slice(&block.to_le_bytes());
        h.extend_from_slice(&spec.bits_per_sample.to_le_bytes());
        if !bext.is_empty() {
            // Taille toujours paire, octet de bourrage compris : certains lecteurs (hound,
            // utilisé pour relire les prises) ignorent le bourrage RIFF des blocs impairs.
            let len = bext.len().next_multiple_of(2);
            h.extend_from_slice(b"bext");
            h.extend_from_slice(&(len as u32).to_le_bytes());
            h.extend_from_slice(bext);
            h.resize(h.len() + len - bext.len(), 0);
        }
        h.extend_from_slice(b"data\0\0\0\0");
        let data_size_at = h.len() as u64 - 4;
        let mut w = BufWriter::with_capacity(WRITE_BUFFER, file);
        w.write_all(&h)?;
        Ok(Self {
            w,
            data_size_at,
            bytes: 0,
            padded: false,
        })
    }

    #[inline]
    fn write_i24(&mut self, v: i32) -> std::io::Result<()> {
        self.bytes += 3;
        self.w.write_all(&v.to_le_bytes()[..3])
    }

    #[inline]
    fn write_f32(&mut self, v: f32) -> std::io::Result<()> {
        self.bytes += 4;
        self.w.write_all(&v.to_le_bytes())
    }

    /// Données sur le disque (tampon vidé) et tailles de l'en-tête à jour.
    fn flush(&mut self) -> std::io::Result<()> {
        let data = self.bytes.min(u32::MAX as u64 - 64);
        let riff = (self.data_size_at + 4 + data + self.padded as u64 - 8).min(u32::MAX as u64);
        self.w.seek(SeekFrom::Start(4))?;
        self.w.write_all(&(riff as u32).to_le_bytes())?;
        self.w.seek(SeekFrom::Start(self.data_size_at))?;
        self.w.write_all(&(data as u32).to_le_bytes())?;
        self.w.seek(SeekFrom::End(0))?;
        self.w.flush()
    }

    /// Fin du fichier : octet de bourrage RIFF si besoin, tailles définitives.
    fn finalize(mut self) -> std::io::Result<()> {
        if self.bytes % 2 == 1 {
            self.w.write_all(&[0])?;
            self.padded = true;
        }
        self.flush()
    }
}

fn open_segment(
    path: &Path,
    wav: WavSpec,
    bext: &dyn Fn(u64) -> Vec<u8>,
    start: u64,
    move_aside: bool,
) -> std::io::Result<SegWriter> {
    // Jamais d'écrasement : si le fichier existe déjà, c'est une erreur signalée — sauf
    // dans la copie de sécurité, où l'ancien fichier homonyme est mis de côté (renommé,
    // jamais supprimé) pour que la copie porte le même nom que l'original.
    let create = || {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
    };
    let file = match create() {
        Err(e) if move_aside && e.kind() == std::io::ErrorKind::AlreadyExists => {
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let mut aside = path.as_os_str().to_os_string();
            aside.push(format!(".ancien-{secs}"));
            std::fs::rename(path, &aside)?;
            create()?
        }
        r => r?,
    };
    let sync = file.try_clone()?;
    let w = TakeWav::create(file, wav, &bext(start))?;
    Ok(SegWriter {
        w,
        file: Arc::new(sync),
        frames: 0,
        start,
    })
}

impl LaneState {
    /// Bloc BWF d'un fichier de la prise commençant à la trame `at` de la prise.
    fn bext(&self, at: u64) -> Vec<u8> {
        let name = self.file.trim_end_matches(".wav");
        crate::encode::bext_bytes(
            name,
            self.time_ref.saturating_add(at),
            self.take_time,
            self.wav.sample_rate,
            self.wav.bits_per_sample,
            self.wav.channels as usize,
        )
    }

    fn fail(&mut self, errors: &Errors, msg: String) {
        errors.lock().push(msg.clone());
        self.error.get_or_insert(msg);
    }

    /// Ferme le fichier en cours (en-tête final + fsync).
    fn close_current(&mut self, errors: &Errors, sync: &Sender<Arc<File>>) {
        if let Some(seg) = self.cur.take() {
            let name = segment_name(&self.file, self.done.len() + 1);
            let mut frames = seg.frames;
            let file = seg.file;
            if let Err(e) = seg.w.finalize() {
                self.fail(errors, format!("{name} : {e}"));
                // Disque plein ou retiré : on recale la longueur sur ce qui est réellement
                // sur le disque (les derniers échantillons en tampon n'y sont peut-être pas).
                if let Ok(real) = crate::recovery::repair_wav(&self.dir.join(&name)) {
                    frames = real.min(frames);
                }
            }
            // Écriture forcée sur le disque dans un thread à part : jamais de blocage ici.
            let _ = sync.send(file);
            self.done.push(Segment {
                file: name,
                frames,
                start: seg.start,
            });
        }
    }

    fn open_next(&mut self, errors: &Errors) {
        let name = segment_name(&self.file, self.done.len() + 1);
        match open_segment(
            &self.dir.join(&name),
            self.wav,
            &|at| self.bext(at),
            self.elapsed,
            self.move_aside,
        ) {
            Ok(mut w) => {
                w.start = self.elapsed;
                self.cur = Some(w);
            }
            Err(e) => {
                self.fail(errors, format!("{name} : {e}"));
                self.retry_at = Some(Instant::now() + RETRY_EVERY);
            }
        }
    }

    /// Après une erreur (disque plein puis libéré, clé rebranchée…) : nouveau fichier,
    /// posé à sa vraie place dans la prise (aucun silence à écrire : les trames perdues
    /// restent un trou sur la timeline, la piste reste calée sur les autres).
    fn retry(&mut self) {
        // Échecs répétés : l'erreur a déjà été signalée, on ne la répète pas chaque seconde.
        let name = segment_name(&self.file, self.done.len() + 1);
        match open_segment(
            &self.dir.join(&name),
            self.wav,
            &|at| self.bext(at),
            self.elapsed,
            self.move_aside,
        ) {
            Ok(mut w) => {
                w.start = self.elapsed;
                self.retry_at = None;
                self.cur = Some(w);
                // Position notée à côté du fichier (récupération après un crash).
                let _ = std::fs::write(
                    self.dir.join(start_sidecar(&name)),
                    self.elapsed.to_string(),
                );
            }
            Err(_) => self.retry_at = Some(Instant::now() + RETRY_EVERY),
        }
    }
}

/// Erreurs signalées avant le démarrage du thread (copie impossible…) ajoutées à celles
/// du thread principal.
fn errors_merge(into: &Errors, early: &Errors) {
    let early = std::mem::take(&mut *early.lock());
    into.lock().splice(0..0, early);
}

impl Recorder {
    #[cfg(test)]
    pub fn start(
        specs: Vec<RecSpec>,
        sample_rate: u32,
        format: RecFormat,
    ) -> std::io::Result<Self> {
        Self::start_with_limit(specs, sample_rate, format, MAX_SEGMENT_BYTES)
    }

    #[cfg(test)]
    pub fn start_with_limit(
        specs: Vec<RecSpec>,
        sample_rate: u32,
        format: RecFormat,
        max_bytes: u64,
    ) -> std::io::Result<Self> {
        Self::spawn(specs, sample_rate, format, max_bytes, None)
    }

    /// Enregistrement avec copie de sécurité simultanée dans `backup` (dossier Audio d'une
    /// session sur un second disque), comme les enregistreurs pro (Reaper, Pyramix, Sound
    /// Devices). La copie a son thread et ses tampons : un second disque lent, plein ou
    /// débranché ne touche jamais l'enregistrement principal.
    pub fn start_with_backup(
        specs: Vec<RecSpec>,
        sample_rate: u32,
        format: RecFormat,
        backup: Option<PathBuf>,
    ) -> std::io::Result<Self> {
        Self::spawn(specs, sample_rate, format, MAX_SEGMENT_BYTES, backup)
    }

    fn spawn(
        specs: Vec<RecSpec>,
        sample_rate: u32,
        format: RecFormat,
        max_bytes: u64,
        backup: Option<PathBuf>,
    ) -> std::io::Result<Self> {
        let errors: Errors = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let copy_lost = Arc::new(AtomicU64::new(0));
        let mut copies = Vec::new();
        let mut backup_rec = None;
        if let Some(dir) = backup {
            // Tampon de la copie : 3 à 10 s dans un budget de 128 Mo.
            let channels: usize = specs.iter().map(|s| s.channels.max(1) as usize).sum();
            let per_sec = sample_rate as usize * channels.max(1) * 4;
            let secs = ((128usize << 20) / per_sec.max(1)).clamp(3, 10);
            let mut bspecs = Vec::new();
            for s in &specs {
                let width = s.channels.max(1) as usize;
                let (prod, cons) = RingBuffer::new(sample_rate as usize * secs * width);
                copies.push(Some(CopyOut {
                    prod,
                    pending: 0,
                    lost: copy_lost.clone(),
                }));
                bspecs.push(RecSpec {
                    track_id: s.track_id,
                    file: s.file.clone(),
                    path: dir.join(&s.file),
                    peaks: Default::default(),
                    cons,
                    channels: s.channels,
                });
            }
            match Self::spawn_writer(bspecs, sample_rate, format, max_bytes, Some(dir), vec![]) {
                Ok(r) => backup_rec = Some(Box::new(r)),
                Err(e) => {
                    copies.clear();
                    errors
                        .lock()
                        .push(format!("copie de sécurité impossible : {e}"));
                }
            }
        }
        let mut me = Self::spawn_writer(specs, sample_rate, format, max_bytes, None, copies)?;
        me.backup = backup_rec;
        me.copy_lost = copy_lost;
        errors_merge(&me.errors, &errors);
        Ok(me)
    }

    /// Thread d'écriture d'un ensemble de pistes. `backup_dir` : copie de sécurité (le
    /// dossier est créé dans ce thread — un second disque en veille ne retarde jamais le
    /// départ de la prise — et un fichier homonyme déjà présent est mis de côté).
    fn spawn_writer(
        specs: Vec<RecSpec>,
        sample_rate: u32,
        format: RecFormat,
        max_bytes: u64,
        backup_dir: Option<PathBuf>,
        copies: Vec<Option<CopyOut>>,
    ) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let errors: Errors = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let errors2 = errors.clone();
        let handle = std::thread::Builder::new()
            .name("tracksplus-disk-writer".into())
            .spawn(move || {
                raise_disk_thread_priority();
                let move_aside = backup_dir.is_some();
                if let Some(d) = &backup_dir {
                    let _ = std::fs::create_dir_all(d);
                }
                run(
                    specs,
                    sample_rate,
                    format,
                    max_bytes,
                    stop2,
                    errors2,
                    copies,
                    move_aside,
                )
            })?;
        Ok(Self {
            stop,
            errors,
            handle: Some(handle),
            backup: None,
            copy_lost: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Copie de sécurité en cours.
    #[cfg(test)]
    pub fn has_backup(&self) -> bool {
        self.backup.is_some()
    }

    /// Erreurs disque survenues depuis le début de la prise (sans doublons).
    pub fn errors(&self) -> Vec<String> {
        let mut v = self.errors.lock().clone();
        v.dedup();
        if let Some(b) = &self.backup {
            for e in b.errors() {
                v.push(format!("copie de sécurité : {e}"));
            }
            if !b.is_alive() && !self.stop.load(Ordering::Relaxed) {
                v.push("copie de sécurité arrêtée (erreur interne)".into());
            }
        }
        let lost = self.copy_lost.load(Ordering::Relaxed);
        if lost > 0 {
            v.push("copie de sécurité : le second disque ne suit pas (silence inséré)".into());
        }
        v
    }

    /// Le thread disque tourne encore. `false` pendant une prise : il s'est arrêté
    /// anormalement (plus rien n'est écrit), l'interface doit le signaler.
    pub fn is_alive(&self) -> bool {
        self.handle.as_ref().is_some_and(|h| !h.is_finished())
    }

    /// Termine les fichiers (à appeler après `Engine::stop_record`).
    /// `Recorder` est `Send` : l'arrêt peut se faire depuis un thread d'arrière-plan.
    /// N'attend jamais indéfiniment : si le disque ne répond plus, on rend la main à
    /// l'interface (les fichiers seront réparés par la récupération).
    pub fn finish(mut self) -> Result<Vec<RecResult>, FinishError> {
        let backup = self.backup.take();
        let res = self.finish_main();
        // La copie se termine à part (son disque peut être lent) : l'enregistrement
        // principal rend la main sans l'attendre.
        if let Some(b) = backup {
            let _ = std::thread::Builder::new()
                .name("tracksplus-backup-finish".into())
                .spawn(move || {
                    let _ = b.finish();
                });
        }
        res
    }

    fn finish_main(&mut self) -> Result<Vec<RecResult>, FinishError> {
        self.stop.store(true, Ordering::Release);
        let Some(h) = self.handle.take() else {
            return Err(FinishError::Panicked);
        };
        let (tx, rx) = crossbeam_channel::bounded(1);
        let _ = std::thread::Builder::new()
            .name("tracksplus-finish".into())
            .spawn(move || {
                let _ = tx.send(h.join());
            });
        match rx.recv_timeout(FINISH_TIMEOUT) {
            Ok(Ok(r)) => Ok(r),
            Ok(Err(_)) => Err(FinishError::Panicked),
            Err(_) => Err(FinishError::Timeout),
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishError {
    /// Le thread disque s'est arrêté anormalement.
    Panicked,
    /// Le disque ne répond plus (retiré, bloqué).
    Timeout,
}

/// Délai maximal d'attente de la fin d'écriture à l'arrêt.
const FINISH_TIMEOUT: Duration = Duration::from_secs(10);

/// Format WAV d'une prise.
fn wav_spec(format: RecFormat, sample_rate: u32, channels: u16) -> WavSpec {
    let (bits_per_sample, sample_format) = match format {
        RecFormat::Int24 => (24, hound::SampleFormat::Int),
        RecFormat::Float32 => (32, hound::SampleFormat::Float),
    };
    WavSpec {
        channels: channels.max(1),
        sample_rate,
        bits_per_sample,
        sample_format,
    }
}

#[allow(clippy::too_many_arguments)]
fn run(
    specs: Vec<RecSpec>,
    sample_rate: u32,
    format: RecFormat,
    max_bytes: u64,
    stop: Arc<AtomicBool>,
    errors: Errors,
    copies: Vec<Option<CopyOut>>,
    move_aside: bool,
) -> Vec<RecResult> {
    let mut copies = copies.into_iter();
    // Heure du début de la prise (la même pour toutes les pistes : elles restent calées
    // entre elles dans un logiciel qui place les fichiers à leur heure).
    let take_time = chrono::Local::now().naive_local();
    let time_ref = {
        use chrono::Timelike;
        let t = take_time.time();
        t.num_seconds_from_midnight() as u64 * sample_rate as u64
            + t.nanosecond() as u64 * sample_rate as u64 / 1_000_000_000
    };
    let mut lanes: Vec<Lane> = specs
        .into_iter()
        .map(|spec| {
            let dir = spec
                .path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default();
            let wav = wav_spec(format, sample_rate, spec.channels);
            let mut st = LaneState {
                track_id: spec.track_id,
                file: spec.file,
                peaks: spec.peaks,
                dir,
                wav,
                k: 0,
                cur: None,
                done: Vec::new(),
                acc: PeakAccum::default(),
                error: None,
                elapsed: 0,
                retry_at: None,
                take_time,
                time_ref,
                move_aside,
            };
            st.open_next(&errors);
            Lane {
                cons: spec.cons,
                st,
                copy: copies.next().flatten(),
            }
        })
        .collect();

    // fsync dans un thread dédié : même lent (disque USB, Windows), il ne bloque jamais
    // le vidage des tampons.
    let (sync_tx, sync_rx) = crossbeam_channel::unbounded::<Arc<File>>();
    let syncer = std::thread::Builder::new()
        .name("tracksplus-fsync".into())
        .spawn(move || {
            for f in sync_rx {
                let _ = f.sync_data();
            }
        })
        .ok();
    let mut last_flush = Instant::now();
    let mut last_sync = Instant::now();
    let mut last_progress = Instant::now();
    let mut peaks_tmp = Vec::with_capacity(1024);
    loop {
        let mut worked = false;
        for lane in &mut lanes {
            let n = lane.cons.slots();
            if n == 0 {
                continue;
            }
            worked = true;
            let Ok(chunk) = lane.cons.read_chunk(n) else {
                continue;
            };
            let st = &mut lane.st;
            let width = st.wav.channels as usize;
            let bytes_per_frame = (st.wav.bits_per_sample / 8) as u64 * width as u64;
            let max_frames = (max_bytes.saturating_sub(1024) / bytes_per_frame).max(1);
            let (a, b) = chunk.as_slices();
            if let Some(c) = &mut lane.copy {
                c.forward(a, b);
            }
            for &s in a.iter().chain(b) {
                // Position dans la trame, indépendante de l'état du fichier : même après
                // une erreur d'écriture, les canaux restent dans le bon ordre.
                let k = st.k;
                st.k = (k + 1) % width;
                st.acc.add(s);
                if k + 1 == width {
                    st.acc.end_frame(&mut peaks_tmp);
                }
                if k == 0 && st.cur.as_ref().is_some_and(|c| c.frames >= max_frames) {
                    // Fichier plein : on passe au suivant (sur une trame entière) sans
                    // interrompre la prise.
                    st.close_current(&errors, &sync_tx);
                    st.open_next(&errors);
                }
                if k == 0 && st.cur.is_none() && st.retry_at.is_some_and(|t| Instant::now() >= t) {
                    st.retry();
                }
                if k + 1 == width {
                    st.elapsed += 1;
                }
                let Some(seg) = st.cur.as_mut() else {
                    continue;
                };
                let r = match format {
                    RecFormat::Int24 => seg
                        .w
                        .write_i24((s.clamp(-1.0, 1.0) * 8_388_607.0).round() as i32),
                    RecFormat::Float32 => seg.w.write_f32(if s.is_finite() { s } else { 0.0 }),
                };
                match r {
                    Ok(()) => {
                        if k + 1 == width {
                            seg.frames += 1;
                        }
                    }
                    Err(e) => {
                        let msg = format!("{} : {e}", st.file);
                        st.fail(&errors, msg);
                        // Le fichier reste valide jusqu'ici : on le ferme proprement, puis
                        // on réessaie régulièrement d'en ouvrir un nouveau.
                        st.close_current(&errors, &sync_tx);
                        st.retry_at = Some(Instant::now() + RETRY_EVERY);
                    }
                }
            }
            chunk.commit_all();
            if !peaks_tmp.is_empty() {
                st.peaks.write().extend(&peaks_tmp);
                peaks_tmp.clear();
            }
        }
        if worked {
            last_progress = Instant::now();
        }

        // En-têtes à jour toutes les 2 s, données forcées sur le disque toutes les 10 s.
        if last_flush.elapsed() > FLUSH_EVERY {
            let sync = last_sync.elapsed() > SYNC_EVERY;
            for lane in &mut lanes {
                let st = &mut lane.st;
                let mut err = None;
                if let Some(seg) = &mut st.cur {
                    if let Err(e) = seg.w.flush() {
                        err = Some(format!("{} : {e}", st.file));
                    } else if sync && Arc::strong_count(&seg.file) == 1 {
                        // Disque lent : la synchronisation précédente n'est pas finie, on
                        // ne l'empile pas (descripteurs de fichiers limités, macOS : 256).
                        let _ = sync_tx.send(seg.file.clone());
                    }
                }
                if let Some(msg) = err {
                    st.fail(&errors, msg);
                }
            }
            last_flush = Instant::now();
            if sync {
                last_sync = Instant::now();
            }
        }

        if stop.load(Ordering::Acquire) {
            let drained = lanes
                .iter()
                .all(|l| l.cons.is_abandoned() && l.cons.is_empty());
            // On n'abandonne que si plus rien n'arrive (carte débranchée, etc.).
            if drained || last_progress.elapsed() > STALL_TIMEOUT {
                break;
            }
        }
        if !worked {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    let results: Vec<RecResult> = lanes
        .into_iter()
        .map(|l| {
            let mut l = l.st;
            l.acc.flush(&mut peaks_tmp);
            l.peaks.write().extend(&peaks_tmp);
            peaks_tmp.clear();
            l.close_current(&errors, &sync_tx);
            RecResult {
                track_id: l.track_id,
                segments: l.done,
                error: l.error,
            }
        })
        .collect();
    // Les dernières écritures forcées sur le disque sont terminées avant de rendre la main.
    drop(sync_tx);
    if let Some(h) = syncer {
        let _ = h.join();
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::{Clip, Project};
    use crate::render::{PeakData, TrackRenderer};
    use parking_lot::RwLock;

    /// Chaîne complète hors carte son : ring buffer → disque → relecture.
    #[test]
    fn record_then_read_back() {
        for format in [RecFormat::Int24, RecFormat::Float32] {
            let dir =
                std::env::temp_dir().join(format!("tp-rec-{}-{format:?}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let mut p = Project::new("R".into(), dir.clone(), 48000);
            p.ensure_dirs().unwrap();
            let id = p.add_track(Some("Voix".into()), Some(0));
            let file = p.new_take_file("Voix");
            let (mut prod, cons) = rtrb::RingBuffer::new(4096);
            let peaks = Arc::new(RwLock::new(PeakData::default()));
            let rec = Recorder::start(
                vec![RecSpec {
                    track_id: id,
                    file: file.clone(),
                    path: p.audio_dir().join(&file),
                    peaks: peaks.clone(),
                    cons,
                    channels: 1,
                }],
                48000,
                format,
            )
            .unwrap();
            let signal: Vec<f32> = (0..20000)
                .map(|i| ((i % 200) as f32 / 100.0) - 1.0)
                .collect();
            let mut i = 0;
            while i < signal.len() {
                if prod.push(signal[i]).is_ok() {
                    i += 1;
                } else {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            drop(prod);
            let res = rec.finish().unwrap();
            assert_eq!(res[0].frames(), 20000);
            assert!(res[0].error.is_none());
            assert_eq!(
                peaks.read().l0.len(),
                20000usize.div_ceil(crate::render::PEAK_BLOCK)
            );

            p.track_mut(id).unwrap().clips.push(Clip {
                id: 5,
                file,
                start: 1000,
                offset: 0,
                len: 20000,
                src_len: 20000,
                fade_in: 0,
                fade_out: 0,
            });
            let mut r = TrackRenderer::new(&p, 0);
            let mut out = vec![9.0f32; 3000];
            r.render(0, &mut out);
            assert!(out[..1000].iter().all(|&s| s == 0.0));
            for k in 0..2000 {
                assert!((out[1000 + k] - signal[k]).abs() < 1e-5, "k={k}");
            }
            // Lecture au-delà de la fin : silence.
            r.render(30000, &mut out);
            assert!(out.iter().all(|&s| s == 0.0));
            std::fs::remove_dir_all(dir).ok();
        }
    }

    /// Une prise longue est répartie sur plusieurs fichiers, sans perte d'échantillon.
    #[test]
    fn long_take_rolls_over_files() {
        let dir = std::env::temp_dir().join(format!("tp-seg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (mut prod, cons) = rtrb::RingBuffer::new(100_000);
        let peaks = Arc::new(RwLock::new(PeakData::default()));
        // Limite de 30 000 octets ≈ 9 650 trames en 24 bits.
        let rec = Recorder::start_with_limit(
            vec![RecSpec {
                track_id: 1,
                file: "T_001.wav".into(),
                path: dir.join("T_001.wav"),
                peaks,
                cons,
                channels: 1,
            }],
            48000,
            RecFormat::Int24,
            30_000,
        )
        .unwrap();
        for i in 0..25_000 {
            prod.push((i % 100) as f32 / 1000.0).unwrap();
        }
        drop(prod);
        let res = rec.finish().unwrap();
        let segs = &res[0].segments;
        assert_eq!(segs.len(), 3);
        assert_eq!(segs[1].file, "T_001_p2.wav");
        assert_eq!(res[0].frames(), 25_000);
        let mut total = 0;
        for sg in segs {
            let r = hound::WavReader::open(dir.join(&sg.file)).unwrap();
            assert_eq!(r.duration() as u64, sg.frames);
            total += sg.frames;
        }
        assert_eq!(total, 25_000);
        std::fs::remove_dir_all(dir).ok();
    }

    /// Stéréo : un WAV 2 canaux, G/D jamais inversés, y compris quand la prise passe
    /// au fichier suivant ; la forme d'onde est comptée en trames.
    #[test]
    fn stereo_take_rolls_over_on_frames() {
        let dir = std::env::temp_dir().join(format!("tp-stereo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (mut prod, cons) = rtrb::RingBuffer::new(100_000);
        let peaks = Arc::new(RwLock::new(PeakData::default()));
        // 30 000 octets ≈ 4 826 trames stéréo 24 bits par fichier.
        let rec = Recorder::start_with_limit(
            vec![RecSpec {
                track_id: 1,
                file: "S_001.wav".into(),
                path: dir.join("S_001.wav"),
                peaks: peaks.clone(),
                cons,
                channels: 2,
            }],
            48000,
            RecFormat::Int24,
            30_000,
        )
        .unwrap();
        let frames = 12_000usize;
        for i in 0..frames {
            let v = (i % 100) as f32 / 1000.0;
            prod.push(v).unwrap();
            prod.push(-v).unwrap();
        }
        drop(prod);
        let res = rec.finish().unwrap();
        assert!(res[0].error.is_none());
        assert_eq!(res[0].frames(), frames as u64);
        assert_eq!(res[0].segments.len(), 3);
        assert_eq!(
            peaks.read().l0.len(),
            frames.div_ceil(crate::render::PEAK_BLOCK)
        );
        let mut n = 0usize;
        for sg in &res[0].segments {
            let mut r = hound::WavReader::open(dir.join(&sg.file)).unwrap();
            assert_eq!(r.spec().channels, 2);
            assert_eq!(r.duration() as u64, sg.frames);
            let s: Vec<i32> = r.samples::<i32>().map(|x| x.unwrap()).collect();
            for f in s.as_chunks::<2>().0 {
                assert_eq!(f[0], -f[1], "trame {n} : canaux inversés");
                let want = ((n % 100) as f32 / 1000.0 * 8_388_607.0).round() as i32;
                assert_eq!(f[0], want, "trame {n}");
                n += 1;
            }
        }
        assert_eq!(n, frames);
        std::fs::remove_dir_all(dir).ok();
    }

    /// Le thread disque est vivant pendant la prise, et l'arrêt peut se faire depuis un
    /// autre thread (`Recorder: Send`).
    #[test]
    fn alive_then_finished_from_another_thread() {
        fn assert_send<T: Send>() {}
        assert_send::<Recorder>();
        let dir = std::env::temp_dir().join(format!("tp-alive-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (mut prod, cons) = rtrb::RingBuffer::new(1000);
        let rec = Recorder::start(
            vec![RecSpec {
                track_id: 1,
                file: "a.wav".into(),
                path: dir.join("a.wav"),
                peaks: Arc::new(RwLock::new(PeakData::default())),
                cons,
                channels: 1,
            }],
            48000,
            RecFormat::Int24,
        )
        .unwrap();
        assert!(rec.is_alive());
        for _ in 0..100 {
            prod.push(0.2).unwrap();
        }
        drop(prod);
        let res = std::thread::spawn(move || rec.finish())
            .join()
            .unwrap()
            .unwrap();
        assert_eq!(res[0].frames(), 100);
        std::fs::remove_dir_all(dir).ok();
    }

    /// Un fichier impossible à créer est signalé tout de suite ; les autres pistes
    /// continuent normalement et l'arrêt se passe sans blocage.
    #[test]
    fn disk_error_is_reported_and_others_continue() {
        let dir = std::env::temp_dir().join(format!("tp-err-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (mut p_ok, c_ok) = rtrb::RingBuffer::new(10_000);
        let (mut p_bad, c_bad) = rtrb::RingBuffer::new(10_000);
        let rec = Recorder::start(
            vec![
                RecSpec {
                    track_id: 1,
                    file: "ok.wav".into(),
                    path: dir.join("ok.wav"),
                    peaks: Arc::new(RwLock::new(PeakData::default())),
                    cons: c_ok,
                    channels: 1,
                },
                RecSpec {
                    track_id: 2,
                    file: "bad.wav".into(),
                    path: dir.join("absent/bad.wav"),
                    peaks: Arc::new(RwLock::new(PeakData::default())),
                    cons: c_bad,
                    channels: 1,
                },
            ],
            48000,
            RecFormat::Int24,
        )
        .unwrap();
        for _ in 0..5000 {
            p_ok.push(0.1).unwrap();
            p_bad.push(0.1).unwrap();
        }
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(rec.errors().len(), 1, "erreur visible pendant la prise");
        drop(p_ok);
        drop(p_bad);
        let t0 = Instant::now();
        let res = rec.finish().unwrap();
        assert!(t0.elapsed() < Duration::from_secs(2));
        assert_eq!(res[0].frames(), 5000);
        assert!(res[0].error.is_none());
        assert!(res[1].error.is_some());
        assert_eq!(res[1].frames(), 0);
        std::fs::remove_dir_all(dir).ok();
    }

    /// Disque revenu (clé rebranchée, place libérée) : la piste reprend dans un nouveau
    /// fichier posé à sa vraie place — elle reste calée sur les autres.
    #[test]
    fn lane_resumes_after_disk_error_and_stays_aligned() {
        let dir = std::env::temp_dir().join(format!("tp-retry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (mut prod, cons) = rtrb::RingBuffer::new(100_000);
        let rec = Recorder::start(
            vec![RecSpec {
                track_id: 1,
                file: "r.wav".into(),
                path: dir.join("r.wav"),
                peaks: Arc::new(RwLock::new(PeakData::default())),
                cons,
                channels: 2,
            }],
            48000,
            RecFormat::Int24,
        )
        .unwrap();
        for _ in 0..3000 * 2 {
            prod.push(0.1).unwrap();
        }
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(rec.errors().len(), 1);
        std::fs::create_dir_all(&dir).unwrap();
        std::thread::sleep(RETRY_EVERY + Duration::from_millis(200));
        for _ in 0..2000 * 2 {
            prod.push(0.1).unwrap();
        }
        drop(prod);
        let res = rec.finish().unwrap();
        // Les 3000 premières trames sont perdues : le fichier repris commence à la trame
        // 3000 de la prise (un trou, pas un décalage), sans silence écrit sur le disque.
        assert_eq!(res[0].frames(), 5000);
        assert_eq!(res[0].written(), 2000);
        assert_eq!(rec_errors_count(&res), 1);
        let seg = &res[0].segments[0];
        assert_eq!((seg.start, seg.frames), (3000, 2000));
        // Position notée à côté du fichier, pour la récupération après un crash.
        let side = std::fs::read_to_string(dir.join(start_sidecar(&seg.file))).unwrap();
        assert_eq!(side, "3000");
        let r = hound::WavReader::open(dir.join(&seg.file)).unwrap();
        assert_eq!(r.duration(), 2000);
        std::fs::remove_dir_all(dir).ok();
    }

    /// Bloc `bext` d'un fichier : (description, TimeReference).
    fn read_bext(path: &Path) -> Option<(String, u64)> {
        let b = std::fs::read(path).ok()?;
        let mut pos = 12;
        while pos + 8 <= b.len() {
            let size = u32::from_le_bytes(b[pos + 4..pos + 8].try_into().ok()?) as usize;
            if &b[pos..pos + 4] == b"bext" {
                let d = &b[pos + 8..pos + 8 + size];
                let desc = String::from_utf8_lossy(&d[..256])
                    .trim_end_matches('\0')
                    .to_string();
                let lo = u32::from_le_bytes(d[338..342].try_into().ok()?) as u64;
                let hi = u32::from_le_bytes(d[342..346].try_into().ok()?) as u64;
                return Some((desc, lo | hi << 32));
            }
            pos += 8 + size + (size & 1);
        }
        None
    }

    /// Prises au format BWF : nom de la piste, heure du début (TimeReference, trames depuis
    /// minuit), et les fichiers suivants d'une prise longue décalés de leur position.
    #[test]
    fn takes_are_bwf_with_time_reference() {
        let dir = std::env::temp_dir().join(format!("tp-bwf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (mut prod, cons) = rtrb::RingBuffer::new(100_000);
        let before = {
            use chrono::Timelike;
            chrono::Local::now().time().num_seconds_from_midnight() as u64
        };
        let rec = Recorder::start_with_limit(
            vec![RecSpec {
                track_id: 1,
                file: "Voix_001.wav".into(),
                path: dir.join("Voix_001.wav"),
                peaks: Arc::new(RwLock::new(PeakData::default())),
                cons,
                channels: 1,
            }],
            48000,
            RecFormat::Int24,
            1024 + 3 * 10_000,
        )
        .unwrap();
        for i in 0..25_000 {
            prod.push((i % 100) as f32 / 200.0).unwrap();
        }
        drop(prod);
        let res = rec.finish().unwrap();
        let segs = &res[0].segments;
        assert!(segs.len() >= 2);
        let (desc, t0) = read_bext(&dir.join(&segs[0].file)).unwrap();
        assert_eq!(desc, "Voix_001");
        let secs = t0 / 48000;
        // Minuit peut tomber pendant le test : on accepte le passage à 0.
        assert!(secs.abs_diff(before) <= 2 || secs < 2, "{secs} / {before}");
        let (_, t1) = read_bext(&dir.join(&segs[1].file)).unwrap();
        assert_eq!(t1, t0 + segs[1].start);
        // Toujours lisible tel quel, échantillons intacts.
        let mut r = hound::WavReader::open(dir.join(&segs[0].file)).unwrap();
        assert_eq!(r.duration() as u64, segs[0].frames);
        let v: Vec<i32> = r.samples::<i32>().take(3).map(|x| x.unwrap()).collect();
        assert_eq!(v[1], (0.005f32 * 8_388_607.0).round() as i32);
        std::fs::remove_dir_all(dir).ok();
    }

    /// Coupure de courant : en-tête resté à la dernière mise à jour, données au-delà sur le
    /// disque — la réparation retrouve tout, bloc BWF compris.
    #[test]
    fn bwf_take_survives_crash() {
        let dir = std::env::temp_dir().join(format!("tp-bwf-crash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("c.wav");
        let file = std::fs::File::create(&path).unwrap();
        let spec = wav_spec(RecFormat::Int24, 48000, 2);
        let mut w = TakeWav::create(file, spec, &[7u8; 603]).unwrap();
        for i in 0..2000 {
            w.write_i24(i).unwrap();
        }
        w.flush().unwrap();
        for i in 0..1001 {
            w.write_i24(i).unwrap();
        }
        // Pas de finalize : le tampon part sur le disque, l'en-tête reste à 1000 trames.
        drop(w);
        assert_eq!(hound::WavReader::open(&path).unwrap().duration(), 1000);
        assert_eq!(crate::recovery::repair_wav(&path).unwrap(), 1500);
        assert_eq!(hound::WavReader::open(&path).unwrap().duration(), 1500);
        std::fs::remove_dir_all(dir).ok();
    }

    /// Copie de sécurité : fichiers identiques sur le second disque ; un second disque
    /// absent n'empêche jamais l'enregistrement principal (erreur signalée).
    #[test]
    fn backup_copy_is_identical_and_never_blocks_main() {
        let base = std::env::temp_dir().join(format!("tp-backup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (main, copy) = (base.join("main"), base.join("copie/Audio"));
        std::fs::create_dir_all(&main).unwrap();
        // Un fichier à la place du dossier parent : la copie ne peut pas être créée.
        std::fs::write(base.join("bloque"), b"x").unwrap();
        // Fichier homonyme d'une ancienne session sur le second disque : mis de côté.
        std::fs::create_dir_all(&copy).unwrap();
        std::fs::write(copy.join("a.wav"), b"ancien").unwrap();
        for (backup, ok) in [(copy.clone(), true), (base.join("bloque/Audio"), false)] {
            let file = if ok { "a.wav" } else { "b.wav" };
            let (mut prod, cons) = rtrb::RingBuffer::new(100_000);
            let rec = Recorder::start_with_backup(
                vec![RecSpec {
                    track_id: 1,
                    file: file.into(),
                    path: main.join(file),
                    peaks: Arc::new(RwLock::new(PeakData::default())),
                    cons,
                    channels: 2,
                }],
                48000,
                RecFormat::Int24,
                Some(backup.clone()),
            )
            .unwrap();
            assert!(rec.has_backup());
            for i in 0..2 * 30_000 {
                prod.push(((i % 97) as f32 - 48.0) / 100.0).unwrap();
            }
            drop(prod);
            if !ok {
                // Erreur signalée par le thread de la copie, sans toucher au principal.
                let t0 = Instant::now();
                while !rec.errors().iter().any(|e| e.contains("copie de sécurité"))
                    && t0.elapsed() < Duration::from_secs(3)
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                assert!(rec.errors().iter().any(|e| e.contains("copie de sécurité")));
            }
            let res = rec.finish().unwrap();
            assert_eq!(res[0].frames(), 30_000);
            assert!(res[0].error.is_none(), "principal intact");
            if ok {
                // La copie se termine dans son propre thread.
                let t0 = Instant::now();
                let read = |p: &Path| -> Option<Vec<i32>> {
                    let mut r = hound::WavReader::open(p).ok()?;
                    (r.duration() == 30_000)
                        .then(|| r.samples::<i32>().map(|x| x.unwrap()).collect())
                };
                let mut got = None;
                while t0.elapsed() < Duration::from_secs(5) && got.is_none() {
                    got = read(&backup.join(file));
                    std::thread::sleep(Duration::from_millis(20));
                }
                assert_eq!(got, read(&main.join(file)), "copie identique");
                let aside = std::fs::read_dir(&backup)
                    .unwrap()
                    .flatten()
                    .any(|e| e.file_name().to_string_lossy().starts_with("a.wav.ancien-"));
                assert!(aside, "ancien fichier gardé de côté");
            }
        }
        std::fs::remove_dir_all(base).ok();
    }

    fn rec_errors_count(res: &[RecResult]) -> usize {
        res.iter().filter(|r| r.error.is_some()).count()
    }
}

#[cfg(test)]
mod load_test {
    use super::*;
    use crate::render::PeakData;
    use parking_lot::RwLock;

    /// 64 pistes à 48 kHz, cadence réelle, blocs de 256 trames (comme un callback ASIO).
    /// `cargo test --release -- --ignored realtime_64_tracks`
    #[test]
    #[ignore]
    fn realtime_64_tracks() {
        realtime_load(64, 1, 48000, 10, RecFormat::Int24);
    }

    /// Charge plus lourde : 64 pistes **stéréo** à 96 kHz en 32 bits flottant
    /// (128 canaux, ≈ 49 Mo/s vers le disque), cadence réelle.
    /// `cargo test --release -- --ignored --nocapture charge_enregistrement`
    #[test]
    #[ignore]
    fn charge_enregistrement_64_stereo_96k() {
        realtime_load(64, 2, 96000, 10, RecFormat::Float32);
    }

    fn realtime_load(tracks: u64, channels: usize, sr: usize, secs: usize, format: RecFormat) {
        let dir =
            std::env::temp_dir().join(format!("tp-load-{}-{tracks}-{sr}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut prods = Vec::new();
        let mut specs = Vec::new();
        for i in 0..tracks {
            let (p, c) = rtrb::RingBuffer::new(sr * 4 * channels);
            prods.push(p);
            specs.push(RecSpec {
                track_id: i,
                file: format!("t{i}.wav"),
                path: dir.join(format!("t{i}.wav")),
                peaks: Arc::new(RwLock::new(PeakData::default())),
                cons: c,
                channels: channels as u16,
            });
        }
        let rec = Recorder::start(specs, sr as u32, format).unwrap();
        let block = 256;
        let start = Instant::now();
        let mut min_free = usize::MAX;
        let mut overruns = 0;
        let mut sent = 0usize;
        while sent < sr * secs {
            let due = Duration::from_secs_f64((sent + block) as f64 / sr as f64);
            if let Some(w) = due.checked_sub(start.elapsed()) {
                std::thread::sleep(w);
            }
            for p in &mut prods {
                min_free = min_free.min(p.slots());
                let n = block * channels;
                if p.slots() < n {
                    overruns += 1;
                    continue;
                }
                let chunk = p.write_chunk_uninit(n).unwrap();
                chunk.fill_from_iter(
                    (0..n).map(|k| ((sent * channels + k) as f32 * 0.01).sin() * 0.5),
                );
            }
            sent += block;
        }
        drop(prods);
        let res = rec.finish().unwrap();
        let mb: u64 = std::fs::read_dir(&dir)
            .map(|d| {
                d.flatten()
                    .filter_map(|e| e.metadata().ok())
                    .map(|m| m.len())
                    .sum()
            })
            .unwrap_or(0)
            / 1_000_000;
        std::fs::remove_dir_all(&dir).ok();
        let cap = sr * 4 * channels;
        println!(
            "enregistrement {tracks} pistes × {channels} canal(aux) à {} kHz : {mb} Mo en {secs} s, \
             tampon jamais rempli à plus de {:.1} %, pertes : {overruns}",
            sr / 1000,
            (cap - min_free) as f64 / cap as f64 * 100.0
        );
        assert_eq!(overruns, 0);
        assert!(res.iter().all(|r| r.frames()
            == (sr * secs).div_ceil(block) as u64 * block as u64
            && r.error.is_none()));
    }
}
