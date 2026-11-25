//! Thread disque : vide les ring buffers d'entrée dans des fichiers WAV (mono ou stéréo).
//!
//! Sécurité « live » :
//! - le journal de secours est écrit par ce thread avant le premier fichier : la capture
//!   démarre sans attendre le disque (les tampons absorbent son réveil) ;
//! - en-têtes WAV mis à jour toutes les 2 s et données forcées sur le disque (fsync)
//!   toutes les 10 s : après un crash, les fichiers sont lisibles (voir `recovery`) ;
//! - au-delà de ~2 Go un fichier est clos et la prise continue dans `nom_p2.wav`,
//!   `nom_p3.wav`… (un WAV ne peut pas dépasser 4 Go) ;
//! - à l'arrêt, tout ce qui reste dans les tampons est écrit avant de fermer, aussi
//!   longtemps que l'écriture avance ;
//! - toute erreur disque est remontée immédiatement à l'interface (liste bornée : une
//!   entrée par piste, avec le nombre de répétitions).

use crate::project::RecFormat;
use crate::recovery::Journal;
use crate::render::{PEAK_BLOCK, PeakAccum, Peaks};
use crossbeam_channel::{Receiver, Sender};
use hound::WavSpec;
use rtrb::Consumer;
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
/// Délai avant le premier essai de réouverture d'un fichier après une erreur d'écriture ;
/// doublé à chaque échec (disque plein : pas un fichier vide par seconde), jusqu'à
/// `RETRY_MAX`, et remis à zéro dès qu'un fichier repris écrit vraiment.
const RETRY_EVERY: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(30);
/// À l'arrêt, abandon seulement si plus rien n'arrive dans les tampons pendant ce délai.
const STALL_TIMEOUT: Duration = Duration::from_secs(3);
const WRITE_BUFFER: usize = 256 * 1024;
/// Nombre maximal d'entrées distinctes dans la liste d'erreurs (une par piste en général).
const MAX_ERROR_ENTRIES: usize = 64;

pub struct RecSpec {
    pub track_id: u64,
    pub file: String,
    pub path: PathBuf,
    pub peaks: Peaks,
    /// Échantillons entrelacés de `channels` canaux (1 = mono, 2 = stéréo).
    pub cons: Consumer<f32>,
    pub channels: u16,
}

/// Piste confiée à un thread d'écriture. Forme d'onde absente pour la copie de sécurité :
/// personne ne l'affiche, inutile de la calculer et de la garder en mémoire.
struct LaneSpec {
    track_id: u64,
    file: String,
    path: PathBuf,
    peaks: Option<Peaks>,
    cons: Consumer<f32>,
    channels: u16,
}

/// Un fichier d'une prise (une prise longue peut en compter plusieurs).
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// Erreurs disque d'une prise, bornées : une entrée par piste (ou par cause générale),
/// avec le nombre de répétitions. Un disque plein pendant des heures ne fait jamais
/// grossir la liste (mémoire, et coût de l'affichage à chaque image).
#[derive(Default, Debug)]
pub struct ErrLog {
    entries: Vec<ErrEntry>,
    /// Erreurs de causes au-delà de `MAX_ERROR_ENTRIES` (seulement comptées).
    dropped: u64,
    /// Change à chaque nouvelle erreur : l'interface ne recalcule son bandeau que dans ce cas.
    generation: u64,
}

#[derive(Debug)]
struct ErrEntry {
    key: String,
    /// Premier message de cette cause (la cause d'origine, pas sa énième répétition).
    msg: String,
    count: u64,
}

impl ErrLog {
    /// Ajoute une erreur de la cause `key` (nom de la piste, ou message général).
    pub fn push(&mut self, key: &str, msg: String) {
        self.generation = self.generation.wrapping_add(1);
        if let Some(e) = self.entries.iter_mut().find(|e| e.key == key) {
            e.count = e.count.saturating_add(1);
        } else if self.entries.len() < MAX_ERROR_ENTRIES {
            self.entries.push(ErrEntry {
                key: key.to_string(),
                msg,
                count: 1,
            });
        } else {
            self.dropped = self.dropped.saturating_add(1);
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Messages lisibles (au plus `MAX_ERROR_ENTRIES` + 1), préfixés de `prefix`.
    fn summary(&self, prefix: &str, out: &mut Vec<String>) {
        for e in &self.entries {
            out.push(if e.count > 1 {
                format!("{prefix}{} (×{})", e.msg, e.count)
            } else {
                format!("{prefix}{}", e.msg)
            });
        }
        if self.dropped > 0 {
            out.push(format!("{prefix}… et {} autre(s) erreur(s)", self.dropped));
        }
    }

    /// Erreurs notées avant celles-ci (copie impossible au démarrage…), placées en tête.
    fn prepend(&mut self, early: ErrLog) {
        let mut all = early;
        for e in std::mem::take(&mut self.entries) {
            if let Some(x) = all.entries.iter_mut().find(|x| x.key == e.key) {
                x.count = x.count.saturating_add(e.count);
            } else if all.entries.len() < MAX_ERROR_ENTRIES {
                all.entries.push(e);
            } else {
                all.dropped = all.dropped.saturating_add(e.count);
            }
        }
        all.dropped = all.dropped.saturating_add(self.dropped);
        all.generation = self.generation.wrapping_add(all.generation).wrapping_add(1);
        *self = all;
    }
}

type Errors = Arc<parking_lot::Mutex<ErrLog>>;

/// Identité de l'état des erreurs d'un enregistrement (principal, copie, copie arrêtée) :
/// tant qu'elle ne change pas, la liste affichée reste la même.
pub type ErrorsGen = (u64, u64, bool);

pub struct Recorder {
    stop: Arc<AtomicBool>,
    /// Erreurs disque survenues pendant la prise (affichées en direct).
    errors: Errors,
    /// Avancement du thread d'écriture (échantillons vidés, fichiers fermés, fsync
    /// terminés) : la fin de prise attend tant que ce compteur bouge.
    progress: Arc<AtomicU64>,
    /// Mises à jour du journal de secours, écrites par le thread d'écriture.
    journal_tx: Sender<(PathBuf, Journal, u64)>,
    handle: Option<JoinHandle<Vec<RecResult>>>,
    /// Copie de sécurité sur un second disque (son propre thread et ses propres tampons).
    backup: Option<Box<Recorder>>,
    /// Recensement du thread d'écriture (arrêt d'urgence).
    #[cfg_attr(not(test), allow(dead_code))]
    slot: Arc<WriterSlot>,
}

struct SegWriter {
    w: TakeWav,
    /// Nom du fichier (dans le dossier de la piste).
    name: String,
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
}

struct LaneState {
    track_id: u64,
    file: String,
    peaks: Option<Peaks>,
    dir: PathBuf,
    wav: WavSpec,
    /// Canal suivant à écrire dans la trame en cours.
    k: usize,
    cur: Option<SegWriter>,
    done: Vec<Segment>,
    /// Numéro du dernier fichier ouvert (le suivant s'appelle `segment_name(file, n + 1)`).
    /// Indépendant de `done` : un fichier vide supprimé ne fait jamais réutiliser son nom.
    seg_no: usize,
    acc: PeakAccum,
    error: Option<String>,
    /// Trames complètes écoulées depuis le début de la prise (écrites ou perdues) : position
    /// du prochain fichier ouvert, pour que la piste reste calée après une erreur.
    elapsed: u64,
    /// Prochain essai de réouverture après une erreur d'écriture.
    retry_at: Option<Instant>,
    /// Délai du prochain essai (doublé à chaque échec).
    retry_delay: Duration,
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
    /// Octets de données acceptés (écrits ou en tampon).
    bytes: u64,
    /// Octet de bourrage final écrit (`finalize`).
    padded: bool,
}

impl TakeWav {
    /// En-tête écrit tout de suite sur le disque : un disque plein échoue ici, au lieu de
    /// laisser un fichier vide qu'une réouverture recréerait sans fin.
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
        w.flush()?;
        Ok(Self {
            w,
            data_size_at,
            bytes: 0,
            padded: false,
        })
    }

    #[inline]
    fn write_i24(&mut self, v: i32) -> std::io::Result<()> {
        self.w.write_all(&v.to_le_bytes()[..3])?;
        self.bytes += 3;
        Ok(())
    }

    #[inline]
    fn write_f32(&mut self, v: f32) -> std::io::Result<()> {
        self.w.write_all(&v.to_le_bytes())?;
        self.bytes += 4;
        Ok(())
    }

    /// Données sur le disque (tampon vidé) et tailles de l'en-tête à jour.
    ///
    /// Le fichier ne reçoit que des ajouts en fin, sauf l'en-tête réécrit sur place : le
    /// tampon est d'abord vidé à la fin du fichier, puis l'en-tête est écrit directement
    /// sur le fichier (jamais via le tampon), et le curseur revient toujours en fin. Une
    /// erreur passagère au milieu (réseau, antivirus) ne peut donc jamais faire écrire
    /// des échantillons par-dessus l'en-tête ou le début de la prise.
    fn flush(&mut self) -> std::io::Result<()> {
        self.w.get_mut().seek(SeekFrom::End(0))?;
        self.w.flush()?;
        let data = self.bytes.min(u32::MAX as u64 - 64);
        let riff = (self.data_size_at + 4 + data + self.padded as u64 - 8).min(u32::MAX as u64);
        let f = self.w.get_mut();
        let header = (|| {
            f.seek(SeekFrom::Start(4))?;
            f.write_all(&(riff as u32).to_le_bytes())?;
            f.seek(SeekFrom::Start(self.data_size_at))?;
            f.write_all(&(data as u32).to_le_bytes())
        })();
        let end = f.seek(SeekFrom::End(0));
        header?;
        end?;
        Ok(())
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
    name: String,
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
            // Nom libre garanti (jamais d'écrasement d'un ancien fichier mis de côté).
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let aside = (0..1000)
                .map(|n| {
                    let mut a = path.as_os_str().to_os_string();
                    a.push(if n == 0 {
                        format!(".ancien-{stamp}")
                    } else {
                        format!(".ancien-{stamp}-{n}")
                    });
                    PathBuf::from(a)
                })
                .find(|a| !a.exists())
                .ok_or_else(|| std::io::Error::other("trop d'anciens fichiers homonymes"))?;
            std::fs::rename(path, &aside)?;
            create()?
        }
        r => r?,
    };
    // Fichier créé mais inutilisable (disque plein : l'en-tête ne passe pas) : supprimé,
    // pour ne pas laisser un fichier vide à chaque essai.
    let sync = match file.try_clone() {
        Ok(s) => s,
        Err(e) => {
            drop(file);
            let _ = std::fs::remove_file(path);
            return Err(e);
        }
    };
    match TakeWav::create(file, wav, &bext(start)) {
        Ok(w) => Ok(SegWriter {
            w,
            name,
            file: Arc::new(sync),
            frames: 0,
            start,
        }),
        Err(e) => {
            drop(sync);
            let _ = std::fs::remove_file(path);
            Err(e)
        }
    }
}

/// Délai de l'essai suivant après un échec (doublement plafonné).
fn next_retry_delay(d: Duration) -> Duration {
    (d * 2).min(RETRY_MAX)
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

    /// Erreur de cette piste : une seule entrée dans la liste, quel que soit le nombre de
    /// répétitions.
    fn fail(&mut self, errors: &Errors, msg: String) {
        errors.lock().push(&self.file, msg.clone());
        self.error.get_or_insert(msg);
    }

    fn schedule_retry(&mut self) {
        self.retry_at = Some(Instant::now() + self.retry_delay);
        self.retry_delay = next_retry_delay(self.retry_delay);
    }

    /// Ferme le fichier en cours (en-tête final + fsync). Seul un fichier réellement vide
    /// (aucune trame acceptée, ou rien après l'en-tête sur le disque) est supprimé : un
    /// fichier dont on ne peut pas relire la longueur (support un instant indisponible,
    /// verrouillé par l'antivirus) est toujours gardé, il peut contenir des heures d'audio.
    fn close_current(&mut self, errors: &Errors, sync: &Sender<Arc<File>>) {
        let Some(seg) = self.cur.take() else {
            return;
        };
        let SegWriter {
            w,
            name,
            file,
            frames,
            start,
        } = seg;
        let path = self.dir.join(&name);
        if frames == 0 {
            // Aucune trame acceptée : rien à garder (prise arrêtée tout de suite).
            drop(w);
            drop(file);
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(self.dir.join(start_sidecar(&name)));
            return;
        }
        let data_start = w.data_size_at + 4;
        let block = (self.wav.bits_per_sample / 8) as u64 * self.wav.channels.max(1) as u64;
        let mut frames = frames;
        if let Err(e) = w.finalize() {
            self.fail(errors, format!("{name} : {e}"));
            // Disque plein ou retiré : on recale la longueur sur ce qui est réellement
            // sur le disque (les derniers échantillons en tampon n'y sont peut-être pas).
            // Réparation impossible : taille du fichier si on peut la lire, sinon la
            // longueur comptée — le fichier est gardé, la récupération recalera.
            frames = match crate::recovery::repair_wav(&path) {
                Ok(real) => real.min(frames),
                Err(_) => std::fs::metadata(&path).map_or(frames, |m| {
                    (m.len().saturating_sub(data_start) / block.max(1)).min(frames)
                }),
            };
            if frames == 0 {
                // Rien après l'en-tête sur le disque (relu avec succès) : fichier vide.
                drop(file);
                let _ = std::fs::remove_file(&path);
                let _ = std::fs::remove_file(self.dir.join(start_sidecar(&name)));
                return;
            }
        }
        // Écriture forcée sur le disque dans un thread à part : jamais de blocage ici.
        let _ = sync.send(file);
        self.done.push(Segment {
            file: name,
            frames,
            start,
        });
    }

    /// Ouvre le fichier suivant de la prise, à la position `elapsed`. `quiet` : échec
    /// déjà signalé (réessais), on ne le répète pas.
    fn open_segment_next(&mut self, errors: &Errors, quiet: bool) -> bool {
        let name = segment_name(&self.file, self.seg_no + 1);
        match open_segment(
            &self.dir.join(&name),
            name.clone(),
            self.wav,
            &|at| self.bext(at),
            self.elapsed,
            self.move_aside,
        ) {
            Ok(w) => {
                self.seg_no += 1;
                self.retry_at = None;
                self.cur = Some(w);
                true
            }
            Err(e) => {
                // Nom déjà pris (fichier vide qui n'a pas pu être supprimé) : le suivant.
                if e.kind() == std::io::ErrorKind::AlreadyExists && !self.move_aside {
                    self.seg_no += 1;
                }
                if !quiet {
                    self.fail(errors, format!("{name} : {e}"));
                }
                self.schedule_retry();
                false
            }
        }
    }

    fn open_next(&mut self, errors: &Errors) {
        self.open_segment_next(errors, false);
    }

    /// Après une erreur (disque plein puis libéré, clé rebranchée…) : nouveau fichier,
    /// posé à sa vraie place dans la prise (aucun silence à écrire : les trames perdues
    /// restent un trou sur la timeline, la piste reste calée sur les autres).
    fn retry(&mut self, errors: &Errors) {
        // Échecs répétés : l'erreur a déjà été signalée, on ne la répète pas.
        if self.open_segment_next(errors, true)
            && let Some(seg) = &self.cur
        {
            // Position notée à côté du fichier (récupération après un crash).
            let _ = std::fs::write(
                self.dir.join(start_sidecar(&seg.name)),
                self.elapsed.to_string(),
            );
        }
    }
}

/// Copie de sécurité en train de se terminer (voir `wait_backups`), avec son avancement
/// et le dernier changement observé (valeur, instant).
struct BackupFinishing {
    handle: JoinHandle<()>,
    progress: Arc<AtomicU64>,
    seen: (u64, Instant),
}

static BACKUP_FINISHING: parking_lot::Mutex<Vec<BackupFinishing>> =
    parking_lot::Mutex::new(Vec::new());

/// Une copie qui n'avance plus depuis ce délai est tenue pour bloquée (second disque qui ne
/// répond plus). Une copie lente qui avance (vidage et fsync sur une clé USB) ne l'est pas.
const BACKUP_STALLED: Duration = Duration::from_secs(20);

/// Retire les copies terminées et dit si l'une des autres n'a fait aucun progrès depuis
/// `idle` (à l'instant `now`).
fn backups_stalled(v: &mut Vec<BackupFinishing>, now: Instant, idle: Duration) -> bool {
    v.retain(|b| !b.handle.is_finished());
    let mut stalled = false;
    for b in v.iter_mut() {
        let p = b.progress.load(Ordering::Relaxed);
        if p != b.seen.0 {
            b.seen = (p, now);
        }
        stalled |= now.saturating_duration_since(b.seen.1) > idle;
    }
    stalled
}

/// Une copie de sécurité d'une prise précédente est bloquée (aucun progrès depuis
/// `BACKUP_STALLED`) : la prise suivante ne doit pas en lancer une autre (ses tampons
/// s'ajouteraient à ceux, jamais libérés, de la copie bloquée). Une copie qui avance
/// encore n'empêche rien : deux jeux de tampons coexistent au plus quelques secondes.
pub fn backup_busy() -> bool {
    backups_stalled(&mut BACKUP_FINISHING.lock(), Instant::now(), BACKUP_STALLED)
}

/// À la fermeture de l'application : laisse les copies de sécurité finir d'écrire, aussi
/// longtemps qu'elles avancent. Abandon après `idle` sans aucun progrès (second disque
/// mort) ou au bout de `cap` en tout.
pub fn wait_backups(idle: Duration, cap: Duration) {
    let t0 = Instant::now();
    let mut seen = (u64::MAX, Instant::now());
    loop {
        let mut v = BACKUP_FINISHING.lock();
        v.retain(|b| !b.handle.is_finished());
        if v.is_empty() {
            return;
        }
        let p = v
            .iter()
            .fold(0u64, |a, b| {
                a.wrapping_add(b.progress.load(Ordering::Relaxed))
            })
            .wrapping_add(v.len() as u64);
        drop(v);
        if p != seen.0 {
            seen = (p, Instant::now());
        }
        if seen.1.elapsed() > idle || t0.elapsed() > cap {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Arrêt d'urgence de tout le processus (fin de session Windows, erreur fatale de
/// l'interface) : voir `emergency_finish`. Définitif : le processus se termine juste après,
/// et une prise qui démarrerait encore s'arrêterait aussitôt.
static EMERGENCY: AtomicBool = AtomicBool::new(false);

/// Un thread d'écriture (prise ou copie de sécurité), recensé tant qu'il vit : l'arrêt
/// d'urgence peut lui être demandé à lui seul (tests) ou à tous (`emergency_finish`).
#[derive(Default)]
struct WriterSlot {
    /// Arrêt d'urgence demandé à ce thread.
    emergency: AtomicBool,
    /// Avancement du thread (trames écrites) : un thread qui écrit encore n'est jamais
    /// abandonné pour un disque mort.
    progress: Arc<AtomicU64>,
    /// Thread terminé (fichiers fermés), même s'il s'est arrêté sur une erreur.
    done: AtomicBool,
}

impl WriterSlot {
    fn emergency_requested(&self) -> bool {
        EMERGENCY.load(Ordering::Acquire) || self.emergency.load(Ordering::Acquire)
    }
}

/// Threads d'écriture encore en vie.
static WRITERS: parking_lot::Mutex<Vec<Arc<WriterSlot>>> = parking_lot::Mutex::new(Vec::new());

/// Recense un thread d'écriture tant qu'il vit (retiré même s'il s'arrête sur une erreur).
struct WriterAlive(Arc<WriterSlot>);

impl WriterAlive {
    fn new(progress: Arc<AtomicU64>) -> Self {
        let slot = Arc::new(WriterSlot {
            progress,
            ..Default::default()
        });
        WRITERS.lock().push(slot.clone());
        Self(slot)
    }
}

impl Drop for WriterAlive {
    fn drop(&mut self) {
        WRITERS.lock().retain(|s| !Arc::ptr_eq(s, &self.0));
        self.0.done.store(true, Ordering::Release);
    }
}

#[cfg(test)]
/// Attend que `finished` soit vrai, au plus `timeout`.
fn wait_until(timeout: Duration, finished: impl Fn() -> bool) -> bool {
    let t0 = Instant::now();
    while !finished() {
        if t0.elapsed() > timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    true
}

/// Arrêt d'urgence, sans passer par l'interface (qui n'est peut-être plus là) : chaque
/// thread d'écriture écrit tout ce qui est déjà arrivé dans ses tampons, ferme ses fichiers
/// (en-têtes WAV à jour) et s'arrête, sans attendre la fin de la prise. Le journal de
/// récupération reste en place : les prises sont réintégrées au prochain lancement.
/// Attend tant que les threads avancent : abandon après `idle` sans aucun progrès, ou
/// `cap` en tout ; `true` si tous ont terminé (copies de sécurité comprises). Sans prise
/// en cours, rend la main tout de suite. Ne s'annule pas : à n'appeler qu'à la fin du
/// processus.
pub fn emergency_finish(idle: Duration, cap: Duration) -> bool {
    if WRITERS.lock().is_empty() {
        return true;
    }
    EMERGENCY.store(true, Ordering::Release);
    let total = || -> u64 {
        WRITERS
            .lock()
            .iter()
            .map(|s| s.progress.load(Ordering::Relaxed))
            .fold(0u64, u64::wrapping_add)
    };
    let t0 = Instant::now();
    let mut seen = (total(), Instant::now());
    while !WRITERS.lock().is_empty() {
        let p = total();
        if p != seen.0 {
            seen = (p, Instant::now());
        }
        if seen.1.elapsed() > idle || t0.elapsed() > cap {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    true
}

/// Arrêt d'urgence de quelques threads seulement (tests : les autres tests du module, qui
/// tournent en parallèle, ne sont pas touchés). Même déroulement que `emergency_finish`.
#[cfg(test)]
fn emergency_finish_slots(slots: &[Arc<WriterSlot>], timeout: Duration) -> bool {
    for s in slots {
        s.emergency.store(true, Ordering::Release);
    }
    wait_until(timeout, || {
        slots.iter().all(|s| s.done.load(Ordering::Acquire))
    })
}

/// Fin de la boucle d'écriture : à l'arrêt, quand plus rien n'arrive (ou que rien n'a
/// avancé depuis `STALL_TIMEOUT`) ; en urgence, dès que ce qui était arrivé est écrit
/// (`emergency` : la dernière passe, faite après la demande, est terminée).
fn writer_should_exit(stop: bool, emergency: bool, drained: bool, stalled: bool) -> bool {
    emergency || (stop && (drained || stalled))
}

/// Fin de la copie de sécurité d'une prise, qui se termine à son rythme.
pub struct BackupFinish {
    pub rx: Receiver<Result<Vec<RecResult>, FinishError>>,
}

/// Résultat de `Recorder::finish`.
pub struct Finished {
    pub main: Result<Vec<RecResult>, FinishError>,
    /// Copie de sécurité en cours de fermeture (son résultat arrive plus tard).
    pub backup: Option<BackupFinish>,
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
        Self::spawn(specs, sample_rate, format, max_bytes, None, None)
    }

    /// Enregistrement avec copie de sécurité simultanée dans `backup` (dossier Audio d'une
    /// session sur un second disque), comme les enregistreurs pro (Reaper, Pyramix, Sound
    /// Devices). La copie a son thread et ses tampons : un second disque lent, plein ou
    /// débranché ne touche jamais l'enregistrement principal.
    /// `journal` : journal de secours (dossier de la session), écrit par le thread
    /// d'écriture avant le premier fichier — jamais par l'interface avant la capture.
    pub fn start_with_backup(
        specs: Vec<RecSpec>,
        sample_rate: u32,
        format: RecFormat,
        backup: Option<(PathBuf, Vec<Consumer<f32>>)>,
        journal: Option<(PathBuf, Journal)>,
    ) -> std::io::Result<Self> {
        Self::spawn(
            specs,
            sample_rate,
            format,
            MAX_SEGMENT_BYTES,
            backup,
            journal,
        )
    }

    fn spawn(
        specs: Vec<RecSpec>,
        sample_rate: u32,
        format: RecFormat,
        max_bytes: u64,
        backup: Option<(PathBuf, Vec<Consumer<f32>>)>,
        journal: Option<(PathBuf, Journal)>,
    ) -> std::io::Result<Self> {
        // Heure du début de la prise, la même pour toutes les pistes et pour la copie de
        // sécurité : tous les fichiers restent calés entre eux dans un logiciel qui les
        // place à leur heure (TimeReference du BWF).
        let take_time = chrono::Local::now().naive_local();
        let time_ref = {
            use chrono::Timelike;
            let t = take_time.time();
            t.num_seconds_from_midnight() as u64 * sample_rate as u64
                + t.nanosecond() as u64 * sample_rate as u64 / 1_000_000_000
        };
        let clock = (take_time, time_ref);
        let mut early = ErrLog::default();
        let mut backup_rec = None;
        if let Some((dir, conss)) = backup {
            // Mêmes pistes, alimentées directement par le callback audio (tampons à
            // part) : la copie ne dépend en rien du disque principal.
            let bspecs: Vec<LaneSpec> = specs
                .iter()
                .zip(conss)
                .map(|(s, cons)| LaneSpec {
                    track_id: s.track_id,
                    file: s.file.clone(),
                    path: dir.join(&s.file),
                    peaks: None,
                    cons,
                    channels: s.channels,
                })
                .collect();
            match Self::spawn_writer(
                bspecs,
                sample_rate,
                format,
                max_bytes,
                Some(dir),
                clock,
                None,
            ) {
                Ok(r) => backup_rec = Some(Box::new(r)),
                Err(e) => {
                    early.push("copie", format!("copie de sécurité impossible : {e}"));
                }
            }
        }
        let specs = specs
            .into_iter()
            .map(|s| LaneSpec {
                track_id: s.track_id,
                file: s.file,
                path: s.path,
                peaks: Some(s.peaks),
                cons: s.cons,
                channels: s.channels,
            })
            .collect();
        let mut me =
            Self::spawn_writer(specs, sample_rate, format, max_bytes, None, clock, journal)?;
        me.backup = backup_rec;
        if early.generation() > 0 {
            me.errors.lock().prepend(early);
        }
        Ok(me)
    }

    /// Thread d'écriture d'un ensemble de pistes. `backup_dir` : copie de sécurité (le
    /// dossier est créé dans ce thread — un second disque en veille ne retarde jamais le
    /// départ de la prise — et un fichier homonyme déjà présent est mis de côté).
    #[allow(clippy::too_many_arguments)]
    fn spawn_writer(
        specs: Vec<LaneSpec>,
        sample_rate: u32,
        format: RecFormat,
        max_bytes: u64,
        backup_dir: Option<PathBuf>,
        clock: (chrono::NaiveDateTime, u64),
        journal: Option<(PathBuf, Journal)>,
    ) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let errors: Errors = Arc::new(parking_lot::Mutex::new(ErrLog::default()));
        let progress = Arc::new(AtomicU64::new(0));
        let (journal_tx, journal_rx) = crossbeam_channel::unbounded();
        // Rang du journal pris maintenant (thread de l'interface) : écrit plus tard par le
        // thread d'écriture, il ne remplace jamais un journal décidé après lui.
        let journal = journal.map(|(d, j)| (d, j, crate::recovery::next_journal_rank()));
        // Compté avant le départ du thread : un arrêt d'urgence juste après l'attend aussi.
        let alive = WriterAlive::new(progress.clone());
        let slot = alive.0.clone();
        let ctx = WriterCtx {
            sample_rate,
            format,
            max_bytes,
            stop: stop.clone(),
            errors: errors.clone(),
            progress: progress.clone(),
            move_aside: backup_dir.is_some(),
            clock,
            journal_rx,
            slot: slot.clone(),
        };
        let handle = std::thread::Builder::new()
            .name("tracksplus-disk-writer".into())
            .spawn(move || {
                let _alive = alive;
                raise_disk_thread_priority();
                if let Some(d) = &backup_dir {
                    let _ = std::fs::create_dir_all(d);
                }
                // Journal de secours avant le premier fichier : après un crash, tout fichier
                // de la prise est connu de la récupération.
                if let Some((dir, j, rank)) = &journal {
                    write_journal_logged(dir, j, *rank, &ctx.errors);
                }
                run(specs, ctx)
            })?;
        Ok(Self {
            stop,
            errors,
            progress,
            journal_tx,
            handle: Some(handle),
            backup: None,
            slot,
        })
    }

    /// Threads d'écriture de la prise (principal, puis copie de sécurité).
    #[cfg(test)]
    fn writer_slots(&self) -> Vec<Arc<WriterSlot>> {
        let mut v = vec![self.slot.clone()];
        if let Some(b) = &self.backup {
            v.extend(b.writer_slots());
        }
        v
    }

    /// Copie de sécurité en cours.
    #[cfg(test)]
    pub fn has_backup(&self) -> bool {
        self.backup.is_some()
    }

    /// Journal de secours complété pendant la prise (décalage d'une prise faite en
    /// écoutant…) : écrit par le thread d'écriture, dans l'ordre, après le journal initial.
    pub fn update_journal(&self, dir: PathBuf, j: Journal) {
        let _ = self
            .journal_tx
            .send((dir, j, crate::recovery::next_journal_rank()));
    }

    /// Compteur d'avancement du thread d'écriture principal (voir `progress`).
    pub fn progress(&self) -> Arc<AtomicU64> {
        self.progress.clone()
    }

    /// Identité de l'état des erreurs : la liste ne change que si elle change.
    pub fn errors_gen(&self) -> ErrorsGen {
        let main = self.errors.lock().generation();
        let (backup, dead) = match &self.backup {
            Some(b) => (
                b.errors.lock().generation(),
                !b.is_alive() && !self.stop.load(Ordering::Relaxed),
            ),
            None => (0, false),
        };
        (main, backup, dead)
    }

    /// Erreurs disque survenues depuis le début de la prise (une ligne par cause, liste
    /// bornée).
    pub fn errors(&self) -> Vec<String> {
        let mut v = Vec::new();
        self.errors.lock().summary("", &mut v);
        if let Some(b) = &self.backup {
            b.errors.lock().summary("copie de sécurité : ", &mut v);
            if !b.is_alive() && !self.stop.load(Ordering::Relaxed) {
                v.push("copie de sécurité arrêtée".into());
            }
        }
        v
    }

    /// Le thread disque tourne encore. `false` pendant une prise : il s'est arrêté
    /// anormalement (plus rien n'est écrit), l'interface doit le signaler.
    pub fn is_alive(&self) -> bool {
        self.handle.as_ref().is_some_and(|h| !h.is_finished())
    }

    /// Termine les fichiers (à appeler après `Engine::stop_record`), hors du thread de
    /// l'interface : attend la vraie fin de l'écriture, aussi longue soit-elle (un disque
    /// lent qui vide 15 s de tampons n'est pas un disque mort). L'appelant surveille
    /// `progress` pour distinguer un disque lent d'un disque bloqué ; le résultat n'est
    /// jamais perdu. La copie de sécurité se termine dans son propre thread.
    pub fn finish(mut self) -> Finished {
        self.stop.store(true, Ordering::Release);
        let backup = self.backup.take().and_then(|b| {
            b.stop.store(true, Ordering::Release);
            let progress = b.progress.clone();
            let (tx, rx) = crossbeam_channel::bounded(1);
            let h = std::thread::Builder::new()
                .name("tracksplus-backup-finish".into())
                .spawn(move || {
                    let _ = tx.send(b.finish().main);
                })
                .ok()?;
            let mut v = BACKUP_FINISHING.lock();
            v.retain(|b| !b.handle.is_finished());
            let seen = (progress.load(Ordering::Relaxed), Instant::now());
            v.push(BackupFinishing {
                handle: h,
                progress,
                seen,
            });
            Some(BackupFinish { rx })
        });
        let main = match self.handle.take() {
            Some(h) => h.join().map_err(|_| FinishError::Panicked),
            None => Err(FinishError::Panicked),
        };
        Finished { main, backup }
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
}

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

/// Trames par fichier : sous la limite d'octets, et multiple de `PEAK_BLOCK` — la forme
/// d'onde calculée en direct se découpe alors exactement par fichier (pas de relecture de
/// toute la prise sur le disque à l'arrêt).
fn segment_frames(max_bytes: u64, bytes_per_frame: u64) -> u64 {
    let block = PEAK_BLOCK as u64;
    (max_bytes.saturating_sub(1024) / bytes_per_frame.max(1) / block * block).max(block)
}

fn write_journal_logged(dir: &Path, j: &Journal, rank: u64, errors: &Errors) {
    if let Err(e) = crate::recovery::write_journal_ranked(dir, j, rank) {
        let msg = format!("journal de secours non écrit : {e:#}");
        errors.lock().push("journal", msg);
    }
}

/// Réglages et liens partagés d'un thread d'écriture.
struct WriterCtx {
    sample_rate: u32,
    format: RecFormat,
    max_bytes: u64,
    stop: Arc<AtomicBool>,
    errors: Errors,
    progress: Arc<AtomicU64>,
    move_aside: bool,
    clock: (chrono::NaiveDateTime, u64),
    journal_rx: Receiver<(PathBuf, Journal, u64)>,
    /// Recensement du thread pour l'arrêt d'urgence.
    slot: Arc<WriterSlot>,
}

fn run(specs: Vec<LaneSpec>, ctx: WriterCtx) -> Vec<RecResult> {
    let WriterCtx {
        sample_rate,
        format,
        max_bytes,
        stop,
        errors,
        progress,
        move_aside,
        clock: (take_time, time_ref),
        journal_rx,
        slot,
    } = ctx;
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
                seg_no: 0,
                acc: PeakAccum::default(),
                error: None,
                elapsed: 0,
                retry_at: None,
                retry_delay: RETRY_EVERY,
                take_time,
                time_ref,
                move_aside,
            };
            st.open_next(&errors);
            Lane {
                cons: spec.cons,
                st,
            }
        })
        .collect();

    // fsync dans un thread dédié : même lent (disque USB, Windows), il ne bloque jamais
    // le vidage des tampons.
    let (sync_tx, sync_rx) = crossbeam_channel::unbounded::<Arc<File>>();
    let sync_progress = progress.clone();
    let syncer = std::thread::Builder::new()
        .name("tracksplus-fsync".into())
        .spawn(move || {
            for f in sync_rx {
                let _ = f.sync_data();
                sync_progress.fetch_add(1, Ordering::Relaxed);
            }
        })
        .ok();
    let mut last_flush = Instant::now();
    let mut last_sync = Instant::now();
    let mut last_progress = Instant::now();
    let mut peaks_tmp = Vec::with_capacity(1024);
    // Arrêt d'urgence vu : encore une passe complète, pour écrire tout ce qui était arrivé
    // dans les tampons au moment de la demande, puis fin.
    let mut last_pass = false;
    loop {
        let mut worked = false;
        // Journal complété par l'interface (rare) : écrit ici, dans l'ordre.
        while let Ok((dir, j, rank)) = journal_rx.try_recv() {
            write_journal_logged(&dir, &j, rank, &errors);
        }
        let stopping = stop.load(Ordering::Acquire);
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
            let max_frames = segment_frames(max_bytes, bytes_per_frame);
            let want_peaks = st.peaks.is_some();
            // Copie de sécurité arrêtée : on vide les tampons sans rouvrir de fichier (un
            // second disque bloqué ne retient pas ses tampons des minutes de plus).
            let may_retry = !(st.move_aside && stopping);
            let (a, b) = chunk.as_slices();
            for &s in a.iter().chain(b) {
                // Position dans la trame, indépendante de l'état du fichier : même après
                // une erreur d'écriture, les canaux restent dans le bon ordre.
                let k = st.k;
                st.k = (k + 1) % width;
                if want_peaks {
                    st.acc.add(s);
                    if k + 1 == width {
                        st.acc.end_frame(&mut peaks_tmp);
                    }
                }
                if k == 0 && st.cur.as_ref().is_some_and(|c| c.frames >= max_frames) {
                    // Fichier plein : on passe au suivant (sur une trame entière) sans
                    // interrompre la prise.
                    st.close_current(&errors, &sync_tx);
                    st.open_next(&errors);
                }
                if k == 0
                    && may_retry
                    && st.cur.is_none()
                    && st.retry_at.is_some_and(|t| Instant::now() >= t)
                {
                    st.retry(&errors);
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
                        let msg = format!("{} : {e}", seg.name);
                        st.fail(&errors, msg);
                        // Le fichier reste valide jusqu'ici : on le ferme proprement, puis
                        // on réessaie d'en ouvrir un nouveau, de plus en plus espacé.
                        st.close_current(&errors, &sync_tx);
                        st.schedule_retry();
                    }
                }
            }
            chunk.commit_all();
            progress.fetch_add(n as u64, Ordering::Relaxed);
            if let Some(p) = &st.peaks
                && !peaks_tmp.is_empty()
            {
                p.write().extend(&peaks_tmp);
            }
            peaks_tmp.clear();
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
                    match seg.w.flush() {
                        Err(e) => err = Some(format!("{} : {e}", seg.name)),
                        Ok(()) => {
                            // Fichier repris qui écrit vraiment : prochains essais rapides.
                            if seg.frames > 0 {
                                st.retry_delay = RETRY_EVERY;
                            }
                            if sync && Arc::strong_count(&seg.file) == 1 {
                                // Disque lent : la synchronisation précédente n'est pas
                                // finie, on ne l'empile pas (descripteurs limités, macOS : 256).
                                let _ = sync_tx.send(seg.file.clone());
                            }
                        }
                    }
                }
                if let Some(msg) = err {
                    // Comme une erreur d'écriture : fichier fermé tel quel (recalé sur ce
                    // qui est vraiment sur le disque), suite dans un nouveau fichier.
                    st.fail(&errors, msg);
                    st.close_current(&errors, &sync_tx);
                    st.schedule_retry();
                }
            }
            progress.fetch_add(1, Ordering::Relaxed);
            last_flush = Instant::now();
            if sync {
                last_sync = Instant::now();
            }
        }

        if !last_pass && slot.emergency_requested() {
            last_pass = true;
            continue;
        }
        let emergency = last_pass;
        if stopping || emergency {
            let drained = lanes
                .iter()
                .all(|l| l.cons.is_abandoned() && l.cons.is_empty());
            // On n'abandonne que si plus rien n'arrive (carte débranchée, etc.), sauf en
            // urgence : ce qui était arrivé vient d'être écrit, on ferme les fichiers.
            let stalled = last_progress.elapsed() > STALL_TIMEOUT;
            if writer_should_exit(stopping, emergency, drained, stalled) {
                break;
            }
        }
        if !worked {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    while let Ok((dir, j, rank)) = journal_rx.try_recv() {
        write_journal_logged(&dir, &j, rank, &errors);
    }

    let results: Vec<RecResult> = lanes
        .into_iter()
        .map(|l| {
            let mut l = l.st;
            if let Some(p) = &l.peaks {
                l.acc.flush(&mut peaks_tmp);
                p.write().extend(&peaks_tmp);
                peaks_tmp.clear();
            }
            l.close_current(&errors, &sync_tx);
            progress.fetch_add(1, Ordering::Relaxed);
            RecResult {
                track_id: l.track_id,
                segments: l.done,
                error: l.error,
            }
        })
        .collect();
    // Les dernières écritures forcées sur le disque sont terminées avant de rendre la main
    // (le thread fsync compte aussi dans l'avancement : un disque lent n'est pas bloqué).
    drop(sync_tx);
    if let Some(h) = syncer {
        let _ = h.join();
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writer_exit_waits_for_the_end_except_in_emergency() {
        // Prise en cours : on continue, même si la source se tait un moment.
        assert!(!writer_should_exit(false, false, false, true));
        assert!(!writer_should_exit(false, false, true, false));
        // Arrêt : seulement une fois tout écrit, ou si plus rien n'avance.
        assert!(!writer_should_exit(true, false, false, false));
        assert!(writer_should_exit(true, false, true, false));
        assert!(writer_should_exit(true, false, false, true));
        // Urgence (fin de session Windows, erreur fatale) : tout de suite, prise en cours
        // ou non, même si l'entrée envoie encore du son.
        assert!(writer_should_exit(false, true, false, false));
        assert!(writer_should_exit(true, true, false, false));
    }
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
            let res = rec.finish().main.unwrap();
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
        let res = rec.finish().main.unwrap();
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
        let res = rec.finish().main.unwrap();
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
        let res = std::thread::spawn(move || rec.finish().main)
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
        let res = rec.finish().main.unwrap();
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
        let res = rec.finish().main.unwrap();
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
        let res = rec.finish().main.unwrap();
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
            // Second tampon, rempli en même temps (par le callback audio dans l'app).
            let (mut cprod, ccons) = rtrb::RingBuffer::new(100_000);
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
                Some((backup.clone(), vec![ccons])),
                None,
            )
            .unwrap();
            assert!(rec.has_backup());
            for i in 0..2 * 30_000 {
                let v = ((i % 97) as f32 - 48.0) / 100.0;
                prod.push(v).unwrap();
                cprod.push(v).unwrap();
            }
            drop(prod);
            drop(cprod);
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
            let res = rec.finish().main.unwrap();
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

    /// Fin d'urgence pendant une prise (entrée toujours branchée, rien n'est arrêté) : tout
    /// ce qui était arrivé est écrit, les fichiers principal et de copie sont lisibles avec
    /// des en-têtes à jour, et plus aucun thread d'écriture de la prise n'est recensé.
    #[test]
    fn emergency_finish_leaves_readable_takes() {
        let base = std::env::temp_dir().join(format!("tp-urgence-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (main, copy) = (base.join("main"), base.join("copie/Audio"));
        std::fs::create_dir_all(&main).unwrap();
        let (mut prod, cons) = rtrb::RingBuffer::new(200_000);
        let (mut cprod, ccons) = rtrb::RingBuffer::new(200_000);
        let rec = Recorder::start_with_backup(
            vec![RecSpec {
                track_id: 1,
                file: "u.wav".into(),
                path: main.join("u.wav"),
                peaks: Arc::new(RwLock::new(PeakData::default())),
                cons,
                channels: 2,
            }],
            48000,
            RecFormat::Int24,
            Some((copy.clone(), vec![ccons])),
            None,
        )
        .unwrap();
        let slots = rec.writer_slots();
        assert_eq!(slots.len(), 2, "prise et copie de sécurité recensées");
        assert!(
            slots
                .iter()
                .all(|s| WRITERS.lock().iter().any(|w| Arc::ptr_eq(w, s)))
        );
        let frames = 40_000;
        for i in 0..2 * frames {
            let v = ((i % 89) as f32 - 44.0) / 100.0;
            prod.push(v).unwrap();
            cprod.push(v).unwrap();
        }
        // Producteurs gardés : la prise est toujours « en cours » côté audio.
        assert!(emergency_finish_slots(&slots, Duration::from_secs(10)));
        assert!(
            slots
                .iter()
                .all(|s| !WRITERS.lock().iter().any(|w| Arc::ptr_eq(w, s))),
            "threads retirés du recensement"
        );
        for path in [main.join("u.wav"), copy.join("u.wav")] {
            let r = hound::WavReader::open(&path).unwrap();
            assert_eq!(r.duration(), frames as u32, "{}", path.display());
            assert_eq!(r.spec().channels, 2);
            assert_eq!(r.spec().bits_per_sample, 24);
            assert_eq!(r.into_samples::<i32>().count(), 2 * frames);
        }
        // Le thread est déjà fini : la fin normale rend la main tout de suite.
        let res = rec.finish().main.unwrap();
        assert_eq!(res[0].frames(), frames as u64);
        drop((prod, cprod));
        std::fs::remove_dir_all(base).ok();
    }

    /// Disque plein pendant des heures : la liste d'erreurs reste bornée (une entrée par
    /// piste avec son nombre de répétitions), et sa génération change à chaque erreur.
    #[test]
    fn error_log_is_bounded_and_counts_repeats() {
        let mut log = ErrLog::default();
        for i in 0..100_000 {
            log.push("Voix_001.wav", format!("Voix_001_p{i}.wav : disque plein"));
        }
        let mut v = Vec::new();
        log.summary("", &mut v);
        assert_eq!(v, ["Voix_001_p0.wav : disque plein (×100000)"]);
        let g = log.generation();
        for i in 0..500 {
            log.push(&format!("piste {i}"), format!("piste {i} : erreur"));
        }
        assert_ne!(log.generation(), g);
        let mut v = Vec::new();
        log.summary("copie : ", &mut v);
        assert_eq!(v.len(), MAX_ERROR_ENTRIES + 1);
        assert!(v.last().unwrap().contains("autre(s) erreur(s)"));
        assert!(v.iter().all(|e| e.starts_with("copie : ")));
        // Erreurs du démarrage placées en tête, sans doublon.
        let mut early = ErrLog::default();
        early.push("copie", "copie impossible".into());
        let mut main = ErrLog::default();
        main.push("a.wav", "a.wav : x".into());
        main.prepend(early);
        let mut v = Vec::new();
        main.summary("", &mut v);
        assert_eq!(v, ["copie impossible", "a.wav : x"]);
    }

    /// Réouvertures de plus en plus espacées après des échecs répétés, plafonnées.
    #[test]
    fn retry_delay_doubles_up_to_a_cap() {
        let mut d = RETRY_EVERY;
        let mut seen = Vec::new();
        for _ in 0..8 {
            seen.push(d.as_secs());
            d = next_retry_delay(d);
        }
        assert_eq!(seen, [1, 2, 4, 8, 16, 30, 30, 30]);
    }

    /// Fichiers d'une prise longue découpés sur une frontière de point de forme d'onde.
    #[test]
    fn segment_length_is_a_multiple_of_peak_block() {
        for (max, bpf) in [(MAX_SEGMENT_BYTES, 3), (MAX_SEGMENT_BYTES, 8), (30_000, 6)] {
            let f = segment_frames(max, bpf);
            assert_eq!(f % PEAK_BLOCK as u64, 0);
            assert!(f * bpf <= max);
        }
        assert_eq!(segment_frames(10, 3), PEAK_BLOCK as u64);
    }

    /// Mise à jour de l'en-tête entre deux écritures : les échantillons suivants vont
    /// toujours en fin de fichier, jamais par-dessus l'en-tête.
    #[test]
    fn header_update_never_overwrites_samples() {
        let dir = std::env::temp_dir().join(format!("tp-hdr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("h.wav");
        let spec = wav_spec(RecFormat::Int24, 48000, 1);
        // En-tête déjà sur le disque à la création.
        let mut w = TakeWav::create(File::create(&path).unwrap(), spec, &[1u8; 33]).unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() > 44);
        for round in 0..5 {
            for i in 0..1000 {
                w.write_i24(round * 1000 + i).unwrap();
            }
            w.flush().unwrap();
        }
        w.finalize().unwrap();
        let mut r = hound::WavReader::open(&path).unwrap();
        let s: Vec<i32> = r.samples::<i32>().map(|x| x.unwrap()).collect();
        assert_eq!(s, (0..5000).collect::<Vec<i32>>());
        std::fs::remove_dir_all(dir).ok();
    }

    /// Prise arrêtée avant la moindre trame : aucun fichier vide laissé sur le disque.
    /// Le journal de secours est écrit par le thread d'écriture, et l'avancement compte.
    #[test]
    fn empty_take_leaves_no_file_and_writer_writes_journal() {
        let dir = std::env::temp_dir().join(format!("tp-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (prod, cons) = rtrb::RingBuffer::new(1000);
        let (mut prod2, cons2) = rtrb::RingBuffer::new(10_000);
        let journal = Journal {
            start: 5,
            play_offset: 0,
            latency: 0,
            sample_rate: 48000,
            extra: Vec::new(),
            takes: vec![crate::recovery::JournalTake {
                track_id: 1,
                file: "e.wav".into(),
            }],
        };
        let spec = |file: &str, cons| RecSpec {
            track_id: 1,
            file: file.into(),
            path: dir.join(file),
            peaks: Arc::new(RwLock::new(PeakData::default())),
            cons,
            channels: 1,
        };
        let rec = Recorder::start_with_backup(
            vec![spec("e.wav", cons), spec("f.wav", cons2)],
            48000,
            RecFormat::Int24,
            None,
            Some((dir.clone(), journal)),
        )
        .unwrap();
        let progress = rec.progress();
        for _ in 0..3000 {
            prod2.push(0.3).unwrap();
        }
        let mut j2 = crate::recovery::Journal {
            start: 5,
            play_offset: 77,
            latency: 3,
            sample_rate: 48000,
            extra: Vec::new(),
            takes: Vec::new(),
        };
        j2.takes.push(crate::recovery::JournalTake {
            track_id: 1,
            file: "e.wav".into(),
        });
        rec.update_journal(dir.clone(), j2);
        drop(prod);
        drop(prod2);
        let res = rec.finish().main.unwrap();
        assert!(res[0].segments.is_empty());
        assert!(!dir.join("e.wav").exists(), "fichier vide supprimé");
        assert_eq!(res[1].frames(), 3000);
        assert!(progress.load(Ordering::Relaxed) >= 3000);
        let text = std::fs::read_to_string(crate::recovery::journal_path(&dir)).unwrap();
        let j: Journal = serde_json::from_str(&text).unwrap();
        assert_eq!(
            (j.play_offset, j.latency),
            (77, 3),
            "dernière version du journal"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    /// Piste mono 24 bits dans `dir`, sans fichier ouvert.
    fn test_lane(dir: &Path) -> LaneState {
        LaneState {
            track_id: 1,
            file: "t.wav".into(),
            peaks: None,
            dir: dir.to_path_buf(),
            wav: wav_spec(RecFormat::Int24, 48000, 1),
            k: 0,
            cur: None,
            done: Vec::new(),
            seg_no: 1,
            acc: PeakAccum::default(),
            error: None,
            elapsed: 0,
            retry_at: None,
            retry_delay: RETRY_EVERY,
            take_time: chrono::Local::now().naive_local(),
            time_ref: 0,
            move_aside: false,
        }
    }

    /// Fichier sans aucune trame : supprimé (avec sa position notée) à la fermeture.
    #[test]
    fn close_current_removes_only_empty_file() {
        let dir = std::env::temp_dir().join(format!("tp-close-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let errors: Errors = Default::default();
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut st = test_lane(&dir);
        let path = dir.join("t.wav");
        st.cur =
            Some(open_segment(&path, "t.wav".into(), st.wav, &|_| Vec::new(), 0, false).unwrap());
        std::fs::write(dir.join(start_sidecar("t.wav")), "0").unwrap();
        st.close_current(&errors, &tx);
        assert!(!path.exists(), "fichier vide supprimé");
        assert!(!dir.join(start_sidecar("t.wav")).exists());
        assert!(st.done.is_empty());
        // Fichier avec des trames : jamais supprimé.
        let path2 = dir.join("u.wav");
        let mut seg =
            open_segment(&path2, "u.wav".into(), st.wav, &|_| Vec::new(), 0, false).unwrap();
        for i in 0..100 {
            seg.w.write_i24(i).unwrap();
            seg.frames += 1;
        }
        st.cur = Some(seg);
        st.close_current(&errors, &tx);
        assert!(path2.exists());
        assert_eq!(st.done.len(), 1);
        assert_eq!(st.done[0].frames, 100);
        assert!(rx.try_recv().is_ok(), "fsync demandé");
        std::fs::remove_dir_all(dir).ok();
    }

    /// Fin de fichier en échec ET réparation impossible (fichier verrouillé, support un
    /// instant indisponible) : le fichier reste sur le disque, avec sa longueur connue.
    #[test]
    fn close_current_keeps_file_when_repair_fails() {
        let dir = std::env::temp_dir().join(format!("tp-close-keep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let errors: Errors = Default::default();
        let (tx, _rx) = crossbeam_channel::unbounded();
        let mut st = test_lane(&dir);
        let path = dir.join("t.wav");
        // Un vrai fichier de 1000 trames sur le disque.
        let mut seg =
            open_segment(&path, "t.wav".into(), st.wav, &|_| Vec::new(), 0, false).unwrap();
        let data_size_at = seg.w.data_size_at;
        for i in 0..1000 {
            seg.w.write_i24(i).unwrap();
        }
        seg.w.flush().unwrap();
        drop(seg);
        std::fs::write(dir.join(start_sidecar("t.wav")), "42").unwrap();
        // Accès en lecture seule : la fin du fichier (en-tête) échoue, et la réparation,
        // qui doit écrire, aussi.
        let orig = std::fs::metadata(&path).unwrap().permissions();
        let mut ro_perm = orig.clone();
        ro_perm.set_readonly(true);
        std::fs::set_permissions(&path, ro_perm).unwrap();
        if std::fs::OpenOptions::new().write(true).open(&path).is_ok() {
            // Droits ignorés (administrateur sous Unix) : cas impossible à simuler ici.
            std::fs::set_permissions(&path, orig).unwrap();
            std::fs::remove_dir_all(dir).ok();
            return;
        }
        let ro = File::open(&path).unwrap();
        st.cur = Some(SegWriter {
            w: TakeWav {
                w: BufWriter::new(ro.try_clone().unwrap()),
                data_size_at,
                bytes: 3000,
                padded: false,
            },
            name: "t.wav".into(),
            file: Arc::new(ro),
            frames: 1000,
            start: 0,
        });
        st.close_current(&errors, &tx);
        assert!(path.exists(), "fichier jamais supprimé");
        assert!(dir.join(start_sidecar("t.wav")).exists(), "position gardée");
        assert_eq!(st.done.len(), 1);
        assert_eq!(st.done[0].frames, 1000, "longueur relue sur le disque");
        assert!(st.error.is_some(), "erreur signalée");
        std::fs::set_permissions(&path, orig).unwrap();
        std::fs::remove_dir_all(dir).ok();
    }

    /// Une copie de sécurité lente qui avance n'est jamais tenue pour bloquée ; une copie
    /// figée l'est après le délai ; une copie terminée disparaît de la liste.
    #[test]
    fn backup_stalled_only_without_progress() {
        let t0 = Instant::now();
        let idle = Duration::from_secs(20);
        let (stop_tx, stop_rx) = crossbeam_channel::unbounded::<()>();
        let spawn = || {
            let rx = stop_rx.clone();
            std::thread::spawn(move || {
                let _ = rx.recv();
            })
        };
        let moving = Arc::new(AtomicU64::new(0));
        let frozen = Arc::new(AtomicU64::new(0));
        let mut v = vec![BackupFinishing {
            handle: spawn(),
            progress: moving.clone(),
            seen: (0, t0),
        }];
        for s in 1..=5u64 {
            moving.fetch_add(1, Ordering::Relaxed);
            assert!(
                !backups_stalled(&mut v, t0 + Duration::from_secs(15 * s), idle),
                "copie lente qui avance"
            );
        }
        v.push(BackupFinishing {
            handle: spawn(),
            progress: frozen.clone(),
            seen: (0, t0 + Duration::from_secs(75)),
        });
        moving.fetch_add(1, Ordering::Relaxed);
        assert!(!backups_stalled(&mut v, t0 + Duration::from_secs(90), idle));
        moving.fetch_add(1, Ordering::Relaxed);
        assert!(backups_stalled(&mut v, t0 + Duration::from_secs(100), idle));
        // Copies terminées : plus rien en attente.
        drop(stop_tx);
        for b in &v {
            while !b.handle.is_finished() {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        assert!(!backups_stalled(
            &mut v,
            t0 + Duration::from_secs(200),
            idle
        ));
        assert!(v.is_empty());
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
        let res = rec.finish().main.unwrap();
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
