//! Thread de lecture : lit les prises sur le disque, les additionne en stéréo
//! et alimente le ring buffer du flux de sortie.

use crate::project::Project;
use crate::render::TrackRenderer;
use rtrb::{Consumer, Producer, RingBuffer};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Trames lues par piste à chaque passe : de gros blocs limitent les allers-retours de
/// la tête de lecture quand beaucoup de fichiers sont sur le même disque.
const BLOCK: usize = 4096;
/// Réserve du tampon de lecture (secondes).
const RING_SECS: usize = 3;
/// Avance minimale avant de lancer la lecture (ms), et attente maximale pour l'obtenir.
const PREFILL_MS: usize = 200;
const PREFILL_WAIT: Duration = Duration::from_millis(400);

pub struct Player {
    /// Piste audible (mute / solo), indexée comme `project.tracks` au démarrage.
    pub audible: Arc<Vec<AtomicBool>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

struct Mixer {
    tracks: Vec<(usize, TrackRenderer)>,
    audible: Arc<Vec<AtomicBool>>,
    pos: u64,
    tbuf: Vec<f32>,
    mix: Vec<f32>,
}

impl Mixer {
    /// Rend un bloc stéréo entrelacé dans le producteur.
    fn fill(&mut self, prod: &mut Producer<f32>) -> bool {
        if prod.slots() < BLOCK * 2 {
            return false;
        }
        self.mix.fill(0.0);
        for (i, t) in &mut self.tracks {
            if !self.audible[*i].load(Ordering::Relaxed) {
                continue;
            }
            let ch = t.channels();
            let buf = &mut self.tbuf[..BLOCK * ch];
            t.render(self.pos, buf);
            if ch == 2 {
                for (m, s) in self.mix.iter_mut().zip(buf.iter()) {
                    *m += *s;
                }
            } else {
                // Piste mono : au centre (même niveau à gauche et à droite).
                for (m, s) in self.mix.as_chunks_mut::<2>().0.iter_mut().zip(buf.iter()) {
                    m[0] += *s;
                    m[1] += *s;
                }
            }
        }
        if let Ok(chunk) = prod.write_chunk_uninit(BLOCK * 2) {
            chunk.fill_from_iter(self.mix.iter().copied());
        }
        self.pos += BLOCK as u64;
        true
    }
}

impl Player {
    /// Prépare la lecture à partir de `pos`. Le consommateur est à donner au moteur.
    pub fn start(project: &Project, pos: u64, audible: Vec<bool>) -> (Self, Consumer<f32>) {
        let sr = project.sample_rate as usize;
        let cap = (sr * RING_SECS).max(BLOCK * 4) * 2;
        let audible: Arc<Vec<AtomicBool>> =
            Arc::new(audible.into_iter().map(AtomicBool::new).collect());
        let (mut prod, cons) = RingBuffer::new(cap);
        let tracks: Vec<_> = (0..project.tracks.len())
            .map(|i| (i, TrackRenderer::new(project, i)))
            .filter(|(_, t)| !t.is_empty())
            .collect();
        let mut mixer = Mixer {
            tracks,
            audible: audible.clone(),
            pos,
            tbuf: vec![0.0; BLOCK * 2],
            mix: vec![0.0; BLOCK * 2],
        };
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let handle = std::thread::Builder::new()
            .name("tracksplus-disk-reader".into())
            .spawn(move || {
                while !stop2.load(Ordering::Acquire) && !prod.is_abandoned() {
                    if !mixer.fill(&mut prod) {
                        std::thread::sleep(Duration::from_millis(3));
                    }
                }
            })
            .expect("thread de lecture");
        // Pré-remplissage (~200 ms) par le thread disque avant de rendre la main : pas de
        // trou au démarrage, et une attente bornée pour ne pas figer l'interface.
        let prefill = (sr * PREFILL_MS / 1000).min(cap / 2 - BLOCK) * 2;
        let deadline = Instant::now() + PREFILL_WAIT;
        while cons.slots() < prefill && Instant::now() < deadline && !handle.is_finished() {
            std::thread::sleep(Duration::from_millis(1));
        }
        (
            Self {
                audible,
                stop,
                handle: Some(handle),
            },
            cons,
        )
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Le thread s'arrête de lui-même dès qu'il voit le drapeau : on n'attend pas (un
        // disque lent figerait l'interface au Stop).
        if let Some(h) = self.handle.take()
            && h.is_finished()
        {
            let _ = h.join();
        }
    }
}
