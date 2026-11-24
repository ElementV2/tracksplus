//! Thread disque : vide les ring buffers d'entrée dans des fichiers WAV mono.

use crate::project::RecFormat;
use crate::render::{PeakAccum, Peaks};
use hound::{WavSpec, WavWriter};
use rtrb::Consumer;
use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub struct RecSpec {
    pub track_id: u64,
    pub file: String,
    pub path: PathBuf,
    pub peaks: Peaks,
    pub cons: Consumer<f32>,
}

#[derive(Debug)]
pub struct RecResult {
    pub track_id: u64,
    pub file: String,
    pub frames: u64,
    pub error: Option<String>,
}

pub struct Recorder {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<Vec<RecResult>>>,
}

struct Lane {
    spec: RecSpec,
    writer: Option<WavWriter<BufWriter<File>>>,
    acc: PeakAccum,
    frames: u64,
    error: Option<String>,
}

impl Recorder {
    pub fn start(specs: Vec<RecSpec>, sample_rate: u32, format: RecFormat) -> Self {
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
        let handle = std::thread::Builder::new()
            .name("tracksplus-disk-writer".into())
            .spawn(move || run(specs, wav, format, stop2))
            .expect("thread d'enregistrement");
        Self {
            stop,
            handle: Some(handle),
        }
    }

    /// Termine les fichiers (à appeler après `Engine::stop_record`).
    pub fn finish(mut self) -> Vec<RecResult> {
        self.stop.store(true, Ordering::Release);
        self.handle
            .take()
            .and_then(|h| h.join().ok())
            .unwrap_or_default()
    }
}

fn run(
    specs: Vec<RecSpec>,
    wav: WavSpec,
    format: RecFormat,
    stop: Arc<AtomicBool>,
) -> Vec<RecResult> {
    let mut lanes: Vec<Lane> = specs
        .into_iter()
        .map(|spec| {
            let (writer, error) = match WavWriter::create(&spec.path, wav) {
                Ok(w) => (Some(w), None),
                Err(e) => (None, Some(format!("{}: {e}", spec.path.display()))),
            };
            Lane {
                spec,
                writer,
                acc: PeakAccum::default(),
                frames: 0,
                error,
            }
        })
        .collect();

    let mut last_flush = Instant::now();
    let mut stop_seen: Option<Instant> = None;
    let mut peaks_tmp = Vec::with_capacity(1024);
    loop {
        let mut worked = false;
        for lane in &mut lanes {
            let n = lane.spec.cons.slots();
            if n == 0 {
                continue;
            }
            worked = true;
            let Ok(chunk) = lane.spec.cons.read_chunk(n) else {
                continue;
            };
            let (a, b) = chunk.as_slices();
            for &s in a.iter().chain(b) {
                lane.acc.push(s, &mut peaks_tmp);
                if let Some(w) = &mut lane.writer {
                    let r = match format {
                        RecFormat::Int24 => {
                            w.write_sample((s.clamp(-1.0, 1.0) * 8_388_607.0).round() as i32)
                        }
                        RecFormat::Float32 => w.write_sample(s),
                    };
                    if let Err(e) = r {
                        lane.error = Some(e.to_string());
                        lane.writer = None;
                    }
                }
            }
            chunk.commit_all();
            lane.frames += n as u64;
            if !peaks_tmp.is_empty() {
                lane.spec.peaks.write().extend(&peaks_tmp);
                peaks_tmp.clear();
            }
        }

        // Mise à jour régulière de l'en-tête WAV : en cas de crash, les fichiers restent lisibles.
        if last_flush.elapsed() > Duration::from_secs(2) {
            for lane in &mut lanes {
                if let Some(w) = &mut lane.writer {
                    let _ = w.flush();
                }
            }
            last_flush = Instant::now();
        }

        if stop.load(Ordering::Acquire) {
            let t = *stop_seen.get_or_insert_with(Instant::now);
            let done = lanes
                .iter()
                .all(|l| l.spec.cons.is_abandoned() && l.spec.cons.is_empty());
            if done || t.elapsed() > Duration::from_secs(2) {
                break;
            }
        }
        if !worked {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    lanes
        .into_iter()
        .map(|mut l| {
            l.acc.flush(&mut peaks_tmp);
            l.spec.peaks.write().extend(&peaks_tmp);
            peaks_tmp.clear();
            let mut error = l.error;
            if let Some(w) = l.writer
                && let Err(e) = w.finalize()
            {
                error.get_or_insert(e.to_string());
            }
            RecResult {
                track_id: l.spec.track_id,
                file: l.spec.file,
                frames: l.frames,
                error,
            }
        })
        .collect()
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
            );
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
            let res = rec.finish();
            assert_eq!(res[0].frames, 20000);
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
        let rec = Recorder::start(specs, sr as u32, RecFormat::Int24);
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
        let res = rec.finish();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(overruns, 0);
        assert!(res.iter().all(
            |r| r.frames == (sr * secs).div_ceil(block) as u64 * block as u64 && r.error.is_none()
        ));
    }
}
