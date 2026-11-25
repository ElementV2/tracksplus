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
use hound::{WavSpec, WavWriter};
use rtrb::Consumer;
use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
}

struct SegWriter {
    w: WavWriter<BufWriter<File>>,
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
}

fn open_segment(path: &Path, wav: WavSpec) -> std::io::Result<SegWriter> {
    // Jamais d'écrasement : si le fichier existe déjà, c'est une erreur signalée.
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    let sync = file.try_clone()?;
    let w = WavWriter::new(BufWriter::with_capacity(WRITE_BUFFER, file), wav)
        .map_err(std::io::Error::other)?;
    Ok(SegWriter {
        w,
        file: Arc::new(sync),
        frames: 0,
        start: 0,
    })
}

impl LaneState {
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
        match open_segment(&self.dir.join(&name), self.wav) {
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
        match open_segment(&self.dir.join(&name), self.wav) {
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

impl Recorder {
    pub fn start(
        specs: Vec<RecSpec>,
        sample_rate: u32,
        format: RecFormat,
    ) -> std::io::Result<Self> {
        Self::start_with_limit(specs, sample_rate, format, MAX_SEGMENT_BYTES)
    }

    pub fn start_with_limit(
        specs: Vec<RecSpec>,
        sample_rate: u32,
        format: RecFormat,
        max_bytes: u64,
    ) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let errors: Errors = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let errors2 = errors.clone();
        let handle = std::thread::Builder::new()
            .name("tracksplus-disk-writer".into())
            .spawn(move || {
                raise_disk_thread_priority();
                run(specs, sample_rate, format, max_bytes, stop2, errors2)
            })?;
        Ok(Self {
            stop,
            errors,
            handle: Some(handle),
        })
    }

    /// Erreurs disque survenues depuis le début de la prise (sans doublons).
    pub fn errors(&self) -> Vec<String> {
        let mut v = self.errors.lock().clone();
        v.dedup();
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

fn run(
    specs: Vec<RecSpec>,
    sample_rate: u32,
    format: RecFormat,
    max_bytes: u64,
    stop: Arc<AtomicBool>,
    errors: Errors,
) -> Vec<RecResult> {
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
                        .write_sample((s.clamp(-1.0, 1.0) * 8_388_607.0).round() as i32),
                    RecFormat::Float32 => seg.w.write_sample(if s.is_finite() { s } else { 0.0 }),
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
