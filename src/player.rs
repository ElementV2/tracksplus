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

    /// Piste muette (mute / solo) : niveau nul sur `frames` trames depuis `pos` (le vumètre
    /// tombe en même temps que le son).
    fn store_silent(&self, track: usize, pos: u64, frames: usize) {
        let Some(slots) = self.slots.get(track) else {
            return;
        };
        for j in 0..frames.div_ceil(SUB) {
            let idx = (pos + (j * SUB) as u64 - self.start) / SUB as u64;
            let s = &slots[(idx % SLOTS) as usize];
            s.tag.store(0, Ordering::Relaxed);
            s.l.store(0, Ordering::Relaxed);
            s.r.store(0, Ordering::Relaxed);
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
    /// Prises illisibles pendant la lecture (fichier absent ou abîmé : jouées en silence).
    pub errors: crossbeam_channel::Receiver<String>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

struct Mixer {
    /// Piste, son rendu, gains (gauche, droite) dans le mix et gain de son fader seul.
    tracks: Vec<(usize, TrackRenderer, [f32; 2], f32)>,
    audible: Arc<Vec<AtomicBool>>,
    meters: Arc<TrackMeters>,
    pos: u64,
    /// Boucle (début, fin) : arrivé à la fin, le rendu reprend au début, sans trou.
    lp: Option<(u64, u64)>,
    tbuf: Vec<f32>,
    mix: Vec<f32>,
    errors: crossbeam_channel::Sender<String>,
}

/// Fondus aux bords d'une boucle (trames) : le son ne saute jamais d'un coup de la fin au
/// début (plage coupée au milieu d'une note), donc aucun clic à chaque tour.
const LOOP_EDGE: u64 = 128;

/// Fondu de sortie avant la fin de la boucle et d'entrée après son début, sur `buf`
/// (stéréo entrelacé) qui commence à la position `start`. Boucles trop courtes : rien.
fn loop_edges(buf: &mut [f32], start: u64, a: u64, b: u64) {
    if b <= a || b - a < 4 * LOOP_EDGE {
        return;
    }
    for (j, f) in buf.as_chunks_mut::<2>().0.iter_mut().enumerate() {
        let p = start + j as u64;
        let ga = if p < a + LOOP_EDGE {
            (p.saturating_sub(a) + 1) as f32 / (LOOP_EDGE + 1) as f32
        } else {
            1.0
        };
        let gb = if p + LOOP_EDGE >= b {
            b.saturating_sub(p) as f32 / (LOOP_EDGE + 1) as f32
        } else {
            1.0
        };
        let g = ga.min(gb);
        if g < 1.0 {
            f[0] *= g;
            f[1] *= g;
        }
    }
}

impl Mixer {
    /// Rend un bloc stéréo entrelacé dans le producteur (en plusieurs morceaux s'il
    /// traverse la fin de la boucle : la suite repart du début, à l'échantillon près).
    fn fill(&mut self, prod: &mut Producer<f32>) -> bool {
        if prod.slots() < BLOCK * 2 {
            return false;
        }
        self.mix.fill(0.0);
        let mut done = 0;
        while done < BLOCK {
            let mut n = BLOCK - done;
            if let Some((a, b)) = self.lp {
                if self.pos >= b {
                    self.pos = a;
                }
                n = n.min((b - self.pos) as usize);
            }
            let start = self.pos;
            self.render_part(done, n);
            if let Some((a, b)) = self.lp {
                loop_edges(&mut self.mix[done * 2..(done + n) * 2], start, a, b);
            }
            self.pos += n as u64;
            done += n;
        }
        if let Ok(chunk) = prod.write_chunk_uninit(BLOCK * 2) {
            chunk.fill_from_iter(self.mix.iter().copied());
        }
        true
    }

    /// Ajoute au mix (à partir de la trame `off`) `n` trames rendues depuis `self.pos`.
    fn render_part(&mut self, off: usize, n: usize) {
        let mix = &mut self.mix[off * 2..(off + n) * 2];
        for (i, t, [gl, gr], fader) in &mut self.tracks {
            if !self.audible[*i].load(Ordering::Relaxed) {
                self.meters.store_silent(*i, self.pos, n);
                continue;
            }
            let ch = t.channels();
            let buf = &mut self.tbuf[..n * ch];
            t.render(self.pos, buf);
            for e in t.take_errors() {
                let _ = self.errors.send(e);
            }
            if ch == 2 {
                // Vumètre après le fader : ce qui part vraiment dans le master.
                for f in buf.as_chunks_mut::<2>().0 {
                    f[0] *= *gl;
                    f[1] *= *gr;
                }
                self.meters.store(*i, self.pos, buf, ch);
                for (m, s) in mix.iter_mut().zip(buf.iter()) {
                    *m += *s;
                }
            } else {
                for x in buf.iter_mut() {
                    *x *= *fader;
                }
                self.meters.store(*i, self.pos, buf, ch);
                // Piste mono : placée par son panoramique (−3 dB de chaque côté au centre).
                let (pl, pr) = if *fader > 0.0 {
                    (*gl / *fader, *gr / *fader)
                } else {
                    (0.0, 0.0)
                };
                for (m, s) in mix.as_chunks_mut::<2>().0.iter_mut().zip(buf.iter()) {
                    m[0] += *s * pl;
                    m[1] += *s * pr;
                }
            }
        }
    }
}

impl Player {
    /// Prépare la lecture à partir de `pos` (en boucle sur `lp` si donnée). Le consommateur
    /// est à donner au moteur.
    pub fn start(
        project: &Project,
        pos: u64,
        audible: Vec<bool>,
        lp: Option<(u64, u64)>,
    ) -> (Self, Consumer<f32>) {
        Self::spawn(project, pos, audible, lp, true)
    }

    /// Nouveau rendu en avance sur la lecture en cours (mute / solo, mix, boucle) : pas
    /// d'attente, le moteur ne bascule que lorsqu'il a assez d'avance.
    pub fn start_ahead(
        project: &Project,
        pos: u64,
        audible: Vec<bool>,
        lp: Option<(u64, u64)>,
    ) -> (Self, Consumer<f32>) {
        Self::spawn(project, pos, audible, lp, false)
    }

    fn spawn(
        project: &Project,
        pos: u64,
        audible: Vec<bool>,
        lp: Option<(u64, u64)>,
        wait: bool,
    ) -> (Self, Consumer<f32>) {
        // Boucle vide ou inversée : pas de boucle.
        let lp = lp.filter(|(a, b)| b > a);
        let sr = project.sample_rate as usize;
        let cap = (sr * RING_SECS).max(BLOCK * 4) * 2;
        let audible: Arc<Vec<AtomicBool>> =
            Arc::new(audible.into_iter().map(AtomicBool::new).collect());
        let (mut prod, cons) = RingBuffer::new(cap);
        let tracks: Vec<_> = (0..project.tracks.len())
            .map(|i| {
                let tr = &project.tracks[i];
                let fader = crate::project::db_to_gain(tr.gain_db);
                (i, TrackRenderer::new(project, i), tr.mix_gains(), fader)
            })
            .filter(|(_, t, _, _)| !t.is_empty())
            .collect();
        // Mesures repérées depuis la position la plus en amont jouée (début de boucle).
        let first = lp.map_or(pos, |(a, _)| a.min(pos));
        let meters = Arc::new(TrackMeters::new(first, project.tracks.len()));
        let (err_tx, errors) = crossbeam_channel::unbounded();
        let mut mixer = Mixer {
            errors: err_tx,
            tracks,
            audible: audible.clone(),
            meters: meters.clone(),
            pos,
            lp,
            tbuf: vec![0.0; BLOCK * 2],
            mix: vec![0.0; BLOCK * 2],
        };
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let handle = std::thread::Builder::new()
            .name("tracksplus-disk-reader".into())
            .spawn(move || {
                crate::recorder::raise_disk_thread_priority();
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
        while wait && cons.slots() < prefill && Instant::now() < deadline && !handle.is_finished() {
            std::thread::sleep(Duration::from_millis(1));
        }
        (
            Self {
                audible,
                meters,
                errors,
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
        // Piste muette : niveau nul (et non « inconnu »).
        m.store_silent(0, 1000 + 2 * BLOCK as u64, BLOCK);
        assert_eq!(
            m.peak(0, 1000 + 2 * BLOCK as u64, 1000 + 2 * BLOCK as u64 + 10),
            Some([0.0, 0.0])
        );
        // Avant le début, ou passage pas encore lu : rien.
        assert_eq!(m.peak(0, 0, 500), None);
        assert_eq!(
            m.peak(0, 1000 + 3 * BLOCK as u64, 1000 + 3 * BLOCK as u64),
            None
        );
        // Piste inconnue : rien, sans panique.
        assert_eq!(m.peak(7, 1000, 2000), None);
    }

    /// Bords de boucle : fondu de sortie avant la fin, d'entrée après le début.
    #[test]
    fn loop_edges_fade_out_then_in() {
        let (a, b) = (1000u64, 3000u64);
        // Fin de boucle : 256 trames avant b, niveau qui descend jusqu'à presque 0.
        let mut end = vec![1.0f32; 256 * 2];
        loop_edges(&mut end, b - 256, a, b);
        assert_eq!(end[0], 1.0);
        assert!(end[255 * 2] < 0.01 && end[255 * 2] > 0.0);
        assert!(end.windows(2).step_by(2).all(|w| w[1] <= w[0] + 1e-6));
        // Début de boucle : remonte depuis presque 0.
        let mut head = vec![1.0f32; 256 * 2];
        loop_edges(&mut head, a, a, b);
        assert!(head[0] < 0.01);
        assert_eq!(head[255 * 2], 1.0);
        // Milieu de boucle, et boucle trop courte : intact.
        let mut mid = vec![1.0f32; 64];
        loop_edges(&mut mid, 2000, a, b);
        assert!(mid.iter().all(|&x| x == 1.0));
        let mut short = vec![1.0f32; 64];
        loop_edges(&mut short, 1000, 1000, 1100);
        assert!(short.iter().all(|&x| x == 1.0));
    }

    /// Boucle : arrivé à la fin de la plage, le rendu reprend au début, à l'échantillon
    /// près, même au milieu d'un bloc et pour une boucle plus courte qu'un bloc.
    #[test]
    fn loop_wraps_at_the_exact_sample() {
        let meters = Arc::new(TrackMeters::new(0, 0));
        let (tx, _rx) = crossbeam_channel::unbounded();
        let mut m = Mixer {
            tracks: Vec::new(),
            audible: Arc::new(Vec::new()),
            meters,
            pos: 900,
            lp: Some((100, 1000)),
            tbuf: vec![0.0; BLOCK * 2],
            mix: vec![0.0; BLOCK * 2],
            errors: tx,
        };
        let (mut prod, _cons) = RingBuffer::new(BLOCK * 8);
        assert!(m.fill(&mut prod));
        // 100 trames jusqu'à la fin, puis la suite depuis 100, sur 900 trames par tour.
        let after = (BLOCK - 100) % 900;
        assert_eq!(m.pos, 100 + after as u64);
        m.lp = Some((0, 10));
        m.pos = 5;
        assert!(m.fill(&mut prod));
        assert_eq!(m.pos, ((5 + BLOCK) % 10) as u64);
    }
}
