//! Thread de lecture : lit les prises sur le disque, les additionne en stéréo
//! et alimente le ring buffer du flux de sortie.

use crate::project::Project;
use crate::render::TrackRenderer;
use rtrb::{Consumer, Producer, RingBuffer};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
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

/// Trames par mesure de niveau (≈ 21 ms à 48 kHz).
const SUB: usize = 1024;
/// Mesures gardées par piste : de quoi couvrir largement l'avance du thread disque.
const SLOTS: u64 = 512;

#[derive(Default)]
struct Slot {
    /// Numéro de la mesure + 1 (0 = vide).
    tag: AtomicU64,
    l: AtomicU32,
    r: AtomicU32,
}

/// Niveaux de lecture par piste, repérés par leur position sur la timeline : le thread
/// disque lit en avance, l'interface affiche le niveau du passage réellement entendu.
pub struct TrackMeters {
    start: u64,
    slots: Vec<Vec<Slot>>,
}

impl TrackMeters {
    fn new(start: u64, tracks: usize) -> Self {
        Self {
            start,
            slots: (0..tracks)
                .map(|_| (0..SLOTS).map(|_| Slot::default()).collect())
                .collect(),
        }
    }

    /// Mesure un bloc rendu (`ch` canaux entrelacés) commençant à `pos`.
    fn store(&self, track: usize, pos: u64, buf: &[f32], ch: usize) {
        let Some(slots) = self.slots.get(track) else {
            return;
        };
        for (j, sub) in buf.chunks(SUB * ch).enumerate() {
            let idx = (pos + (j * SUB) as u64 - self.start) / SUB as u64;
            let (mut l, mut r) = (0f32, 0f32);
            if ch == 2 {
                for f in sub.as_chunks::<2>().0 {
                    l = l.max(f[0].abs());
                    r = r.max(f[1].abs());
                }
            } else {
                l = sub.iter().fold(0f32, |m, x| m.max(x.abs()));
                r = l;
            }
            let s = &slots[(idx % SLOTS) as usize];
            s.tag.store(0, Ordering::Relaxed);
            s.l.store(l.to_bits(), Ordering::Relaxed);
            s.r.store(r.to_bits(), Ordering::Relaxed);
            s.tag.store(idx + 1, Ordering::Release);
        }
    }

    /// Crête (gauche, droite) de la piste `track` entendue entre les positions `from` et
    /// `to` ; `None` si rien n'a été joué (piste muette, pas encore lue).
    pub fn peak(&self, track: usize, from: u64, to: u64) -> Option<[f32; 2]> {
        let slots = self.slots.get(track)?;
        if to < self.start {
            return None;
        }
        let b = (to - self.start) / SUB as u64;
        let a = (from.clamp(self.start, to) - self.start) / SUB as u64;
        let a = a.max(b.saturating_sub(32));
        let mut out: Option<[f32; 2]> = None;
        for idx in a..=b {
            let s = &slots[(idx % SLOTS) as usize];
            if s.tag.load(Ordering::Acquire) == idx + 1 {
                let v = [
                    f32::from_bits(s.l.load(Ordering::Relaxed)),
                    f32::from_bits(s.r.load(Ordering::Relaxed)),
                ];
                out = Some(out.map_or(v, |o| [o[0].max(v[0]), o[1].max(v[1])]));
            }
        }
        out
    }
}

pub struct Player {
    /// Piste audible (mute / solo), indexée comme `project.tracks` au démarrage.
    pub audible: Arc<Vec<AtomicBool>>,
    /// Niveaux de lecture par piste (même indexation).
    pub meters: Arc<TrackMeters>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

struct Mixer {
    tracks: Vec<(usize, TrackRenderer)>,
    audible: Arc<Vec<AtomicBool>>,
    meters: Arc<TrackMeters>,
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
            self.meters.store(*i, self.pos, buf, ch);
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
        let meters = Arc::new(TrackMeters::new(pos, project.tracks.len()));
        let mut mixer = Mixer {
            tracks,
            audible: audible.clone(),
            meters: meters.clone(),
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
                meters,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Le niveau affiché est celui du passage entendu, pas celui lu en avance.
    #[test]
    fn meters_follow_the_heard_position() {
        let m = TrackMeters::new(1000, 2);
        // Piste 0 : silence de 1000 à 1000 + 4096, fort ensuite (bloc lu en avance).
        m.store(0, 1000, &vec![0.0; BLOCK], 1);
        m.store(0, 1000 + BLOCK as u64, &vec![0.8; BLOCK], 1);
        // Piste 1 stéréo : gauche 0,5, droite 0,25.
        let st: Vec<f32> = (0..BLOCK * 2)
            .map(|i| if i % 2 == 0 { 0.5 } else { -0.25 })
            .collect();
        m.store(1, 1000, &st, 2);
        assert_eq!(m.peak(0, 1000, 1500), Some([0.0, 0.0]));
        assert_eq!(m.peak(0, 1500, 1000 + BLOCK as u64 + 10), Some([0.8, 0.8]));
        assert_eq!(m.peak(1, 1000, 2000), Some([0.5, 0.25]));
        // Avant le début, ou passage pas encore lu : rien.
        assert_eq!(m.peak(0, 0, 500), None);
        assert_eq!(
            m.peak(0, 1000 + 3 * BLOCK as u64, 1000 + 3 * BLOCK as u64),
            None
        );
        // Piste inconnue : rien, sans panique.
        assert_eq!(m.peak(7, 1000, 2000), None);
    }
}
