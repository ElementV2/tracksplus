//! Thread disque : vide les ring buffers d'entrée dans des fichiers WAV mono.
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
const STALL_TIMEOUT: Duration = Duration::from_secs(3);
const WRITE_BUFFER: usize = 256 * 1024;

pub struct RecSpec {
    pub track_id: u64,
    pub file: String,
    pub path: PathBuf,
    pub peaks: Peaks,
    pub cons: Consumer<f32>,
}

/// Un fichier d'une prise (une prise longue peut en compter plusieurs).
#[derive(Debug, Clone)]
pub struct Segment {
    pub file: String,
    pub frames: u64,
}

#[derive(Debug)]
pub struct RecResult {
    pub track_id: u64,
    pub segments: Vec<Segment>,
    pub error: Option<String>,
}

impl RecResult {
    pub fn frames(&self) -> u64 {
        self.segments.iter().map(|s| s.frames).sum()
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
    /// Second accès au fichier pour forcer l'écriture sur le disque.
    file: File,
    frames: u64,
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
    cur: Option<SegWriter>,
    done: Vec<Segment>,
    acc: PeakAccum,
    error: Option<String>,
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
        file: sync,
        frames: 0,
    })
}

impl LaneState {
    fn fail(&mut self, errors: &Errors, msg: String) {
        errors.lock().push(msg.clone());
        self.error.get_or_insert(msg);
    }

    /// Ferme le fichier en cours (en-tête final + fsync).
    fn close_current(&mut self, errors: &Errors, sync: &Sender<File>) {
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
            self.done.push(Segment { file: name, frames });
        }
    }

    fn open_next(&mut self, wav: WavSpec, errors: &Errors) {
        let name = segment_name(&self.file, self.done.len() + 1);
        match open_segment(&self.dir.join(&name), wav) {
            Ok(w) => self.cur = Some(w),
            Err(e) => self.fail(errors, format!("{name} : {e}")),
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
        let wav = match format {
            RecFormat::Int24 => WavSpec {
                channels: 1,
                sample_rate,
                bits_per_sample: 24,
                sample_format: hound::SampleFormat::Int,
            },
            RecFormat::Float32 => WavSpec {
                channels: 1,
                sample_rate,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            },
        };
        let errors: Errors = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let errors2 = errors.clone();
        let handle = std::thread::Builder::new()
            .name("tracksplus-disk-writer".into())
            .spawn(move || run(specs, wav, format, max_bytes, stop2, errors2))?;
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

    /// Termine les fichiers (à appeler après `Engine::stop_record`).
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

fn run(
    specs: Vec<RecSpec>,
    wav: WavSpec,
    format: RecFormat,
    max_bytes: u64,
    stop: Arc<AtomicBool>,
    errors: Errors,
) -> Vec<RecResult> {
    let bytes_per_frame = (wav.bits_per_sample / 8) as u64;
    let max_frames = (max_bytes.saturating_sub(1024) / bytes_per_frame).max(1);
    let mut lanes: Vec<Lane> = specs
        .into_iter()
        .map(|spec| {
            let dir = spec
                .path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default();
            let mut st = LaneState {
                track_id: spec.track_id,
                file: spec.file,
                peaks: spec.peaks,
                dir,
                cur: None,
                done: Vec::new(),
                acc: PeakAccum::default(),
                error: None,
            };
            st.open_next(wav, &errors);
            Lane {
                cons: spec.cons,
                st,
            }
        })
        .collect();

    // fsync dans un thread dédié : même lent (disque USB, Windows), il ne bloque jamais
    // le vidage des tampons.
    let (sync_tx, sync_rx) = crossbeam_channel::unbounded::<File>();
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
            let (a, b) = chunk.as_slices();
            for &s in a.iter().chain(b) {
                st.acc.push(s, &mut peaks_tmp);
                if st.cur.as_ref().is_some_and(|c| c.frames >= max_frames) {
                    // Fichier plein : on passe au suivant sans interrompre la prise.
                    st.close_current(&errors, &sync_tx);
                    st.open_next(wav, &errors);
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
                    Ok(()) => seg.frames += 1,
                    Err(e) => {
                        let msg = format!("{} : {e}", st.file);
                        st.fail(&errors, msg);
                        // Le fichier reste valide jusqu'ici : on le ferme proprement.
                        st.close_current(&errors, &sync_tx);
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
                    } else if sync && let Ok(f) = seg.file.try_clone() {
                        let _ = sync_tx.send(f);
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
                },
                RecSpec {
                    track_id: 2,
                    file: "bad.wav".into(),
                    path: dir.join("absent/bad.wav"),
                    peaks: Arc::new(RwLock::new(PeakData::default())),
                    cons: c_bad,
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
        let dir = std::env::temp_dir().join(format!("tp-load-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sr = 48000usize;
        let mut prods = Vec::new();
        let mut specs = Vec::new();
        for i in 0..64 {
            let (p, c) = rtrb::RingBuffer::new(sr * 4);
            prods.push(p);
            specs.push(RecSpec {
                track_id: i,
                file: format!("t{i}.wav"),
                path: dir.join(format!("t{i}.wav")),
                peaks: Arc::new(RwLock::new(PeakData::default())),
                cons: c,
            });
        }
        let rec = Recorder::start(specs, sr as u32, RecFormat::Int24).unwrap();
        let block = 256;
        let secs = 10;
        let start = Instant::now();
        let mut overruns = 0;
        let mut sent = 0usize;
        while sent < sr * secs {
            let due = Duration::from_secs_f64((sent + block) as f64 / sr as f64);
            if let Some(w) = due.checked_sub(start.elapsed()) {
                std::thread::sleep(w);
            }
            for p in &mut prods {
                if p.slots() < block {
                    overruns += 1;
                    continue;
                }
                let chunk = p.write_chunk_uninit(block).unwrap();
                chunk.fill_from_iter((0..block).map(|k| ((sent + k) as f32 * 0.01).sin() * 0.5));
            }
            sent += block;
        }
        drop(prods);
        let res = rec.finish().unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(overruns, 0);
        assert!(res.iter().all(|r| r.frames()
            == (sr * secs).div_ceil(block) as u64 * block as u64
            && r.error.is_none()));
    }
}
