//! Thread de lecture : lit les prises sur le disque, les additionne en stéréo
//! et alimente le ring buffer du flux de sortie.

use crate::project::Project;
use crate::render::TrackRenderer;
use rtrb::{Consumer, Producer, RingBuffer};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

const BLOCK: usize = 1024;

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
            t.render(self.pos, &mut self.tbuf);
            for (m, s) in self.mix.iter_mut().zip(&self.tbuf) {
                *m += *s;
            }
        }
        if let Ok(chunk) = prod.write_chunk_uninit(BLOCK * 2) {
            let mix = &self.mix;
            chunk.fill_from_iter(mix.iter().flat_map(|&s| [s, s]));
        }
        self.pos += BLOCK as u64;
        true
    }
}

impl Player {
    /// Prépare la lecture à partir de `pos`. Le consommateur est à donner au moteur.
    pub fn start(project: &Project, pos: u64, audible: Vec<bool>) -> (Self, Consumer<f32>) {
        let cap = (project.sample_rate as usize / 2).max(BLOCK * 4) * 2;
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
            tbuf: vec![0.0; BLOCK],
            mix: vec![0.0; BLOCK],
        };
        // Pré-remplissage pour éviter un trou au démarrage.
        // Petit pré-remplissage sur le thread appelant (le reste se fait en arrière-plan),
        // pour ne pas bloquer l'interface à chaque déplacement de la tête.
        for _ in 0..2 {
            mixer.fill(&mut prod);
        }
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
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
