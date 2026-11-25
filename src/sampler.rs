//! Moteur du soundboard, exécuté dans le callback de sortie audio.
//!
//! Les sons sont en mémoire (stéréo entrelacé, à la fréquence de la carte). Le callback
//! reçoit des commandes (appui / relâchement de pad, stop) et mixe les voix actives.
//! Aucune allocation ni libération n'a lieu dans le thread audio : les sons terminés
//! sont rendus à l'interface via un canal « poubelle ».

use crossbeam_channel::{Receiver, Sender};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Nombre de pads par page et nombre maximal de pages.
pub const PADS_PER_PAGE: usize = 64;
pub const MAX_PAGES: usize = 8;
pub const MAX_PADS: usize = PADS_PER_PAGE * MAX_PAGES;
const MAX_VOICES: usize = 64;

/// Son chargé en mémoire.
pub struct SampleData {
    pub sample_rate: u32,
    pub frames: usize,
    /// Stéréo entrelacé.
    pub data: Vec<f32>,
    /// Forme d'onde (min, max) par bloc de `SAMPLE_PEAK` trames.
    pub peaks: Vec<[f32; 2]>,
}

pub const SAMPLE_PEAK: usize = 256;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum PadMode {
    /// Un appui lance le son, un nouvel appui l'arrête.
    #[default]
    #[serde(alias = "OneShot")]
    Toggle,
    /// Le son joue tant que le pad est maintenu et s'arrête au relâchement.
    Gate,
    /// Chaque appui relance le son depuis le début (même s'il joue déjà).
    Retrigger,
}

/// Paramètres de lecture d'un pad, en trames.
#[derive(Clone, Copy, Debug)]
pub struct PadParams {
    pub start: usize,
    pub end: usize,
    pub fade_in: usize,
    pub fade_out: usize,
    pub gain: f32,
    pub mode: PadMode,
    pub looped: bool,
}

pub enum PadCmd {
    Press {
        slot: u16,
        sample: Arc<SampleData>,
        params: PadParams,
    },
    Release {
        slot: u16,
    },
    Stop {
        slot: u16,
    },
    /// Arrêt immédiat de toutes les voix (micro-fondu anti-clic de 10 ms).
    StopAll,
    /// Panique « douce » : toutes les voix s'éteignent en fondu sur `frames` trames.
    FadeAll {
        frames: usize,
    },
    SetOutput {
        left: usize,
    },
}

struct Voice {
    slot: u16,
    sample: Arc<SampleData>,
    p: PadParams,
    pos: usize,
    /// Fondu de relâchement : (trames restantes, total).
    release: Option<(usize, usize)>,
    /// Premier passage (le fondu d'entrée ne s'applique qu'une fois en boucle).
    first: bool,
}

impl Voice {
    fn release(&mut self, fade: usize) {
        let fade = fade.max(64);
        match self.release {
            Some((rem, _)) if rem <= fade => {}
            _ => self.release = Some((fade, fade)),
        }
    }
}

fn smooth(x: f32) -> f32 {
    // Courbe en S (sinus²) : fondus doux et sans clic.
    let x = x.clamp(0.0, 1.0);
    let s = (x * std::f32::consts::FRAC_PI_2).sin();
    s * s
}

/// Deux appuis sur le même pad à moins de 150 ms : rebond d'un bouton MIDI ou double-clic
/// involontaire — le second est ignoré (sinon un pad « appui = lecture / appui = stop »
/// s'arrêterait aussitôt lancé). Comme la protection « double GO » des logiciels de
/// spectacle.
const DEBOUNCE_MS: usize = 150;

pub struct Sampler {
    voices: Vec<Voice>,
    /// Trames jouées depuis le démarrage (horloge du soundboard).
    clock: u64,
    /// Horloge du dernier appui pris en compte, par pad (alloué une fois).
    last_press: Vec<u64>,
    rx: Receiver<PadCmd>,
    garbage: Sender<Arc<SampleData>>,
    pub left: usize,
    sample_rate: usize,
}

impl Sampler {
    pub fn new(
        rx: Receiver<PadCmd>,
        garbage: Sender<Arc<SampleData>>,
        left: usize,
        sample_rate: u32,
    ) -> Self {
        Self {
            voices: Vec::with_capacity(MAX_VOICES),
            clock: 0,
            last_press: vec![0; MAX_PADS],
            rx,
            garbage,
            left,
            sample_rate: sample_rate as usize,
        }
    }

    fn dispose(&self, s: Arc<SampleData>) {
        if let Err(e) = self.garbage.try_send(s) {
            std::mem::forget(e.into_inner());
        }
    }

    fn quick(&self) -> usize {
        self.sample_rate / 100 // 10 ms
    }

    fn handle(&mut self, cmd: PadCmd) {
        let quick = self.quick();
        match cmd {
            PadCmd::Press {
                slot,
                sample,
                params,
            } => {
                let debounce = (self.sample_rate * DEBOUNCE_MS / 1000) as u64;
                if params.mode != PadMode::Gate
                    && let Some(last) = self.last_press.get_mut(slot as usize)
                {
                    if *last != 0 && self.clock.saturating_sub(*last) < debounce {
                        self.dispose(sample);
                        return;
                    }
                    *last = self.clock.max(1);
                }
                let active = self
                    .voices
                    .iter()
                    .any(|v| v.slot == slot && v.release.is_none());
                match params.mode {
                    PadMode::Toggle if active => {
                        for v in self.voices.iter_mut().filter(|v| v.slot == slot) {
                            v.release(params.fade_out.max(quick));
                        }
                        self.dispose(sample);
                        return;
                    }
                    PadMode::Gate if active => {
                        self.dispose(sample);
                        return;
                    }
                    PadMode::Retrigger if active => {
                        // Le son en cours s'efface en 10 ms (pas de clic) pendant que le
                        // nouveau repart du début.
                        for v in self
                            .voices
                            .iter_mut()
                            .filter(|v| v.slot == slot && v.release.is_none())
                        {
                            v.release(quick);
                        }
                    }
                    _ => {}
                }
                if self.voices.len() >= MAX_VOICES || params.start >= params.end {
                    self.dispose(sample);
                    return;
                }
                self.voices.push(Voice {
                    slot,
                    pos: params.start,
                    p: params,
                    sample,
                    release: None,
                    first: true,
                });
            }
            PadCmd::Release { slot } => {
                for v in self
                    .voices
                    .iter_mut()
                    .filter(|v| v.slot == slot && v.p.mode == PadMode::Gate)
                {
                    let f = v.p.fade_out.min(quick * 50).max(quick);
                    v.release(f);
                }
            }
            PadCmd::Stop { slot } => {
                for v in self.voices.iter_mut().filter(|v| v.slot == slot) {
                    v.release(quick * 2);
                }
            }
            PadCmd::StopAll => {
                for v in &mut self.voices {
                    v.release(quick);
                }
            }
            PadCmd::FadeAll { frames } => {
                for v in &mut self.voices {
                    v.release(frames.max(quick));
                }
            }
            PadCmd::SetOutput { left } => self.left = left,
        }
    }

    /// Mixe les voix dans `out` (stéréo entrelacé, `frames` trames) en additionnant.
    pub fn render(&mut self, out: &mut [f32], frames: usize, pad_pos: &[AtomicU64]) {
        while let Ok(cmd) = self.rx.try_recv() {
            self.handle(cmd);
        }
        self.clock += frames as u64;
        for v in &mut self.voices {
            let data = &v.sample.data;
            let p = v.p;
            let end = p.end.min(v.sample.frames);
            for f in 0..frames {
                if v.pos >= end {
                    if p.looped && v.release.is_none() {
                        v.pos = p.start;
                        v.first = false;
                    } else {
                        v.release = Some((0, 1));
                        break;
                    }
                }
                let mut g = p.gain;
                if v.first && p.fade_in > 0 {
                    let k = v.pos.saturating_sub(p.start);
                    if k < p.fade_in {
                        g *= smooth(k as f32 / p.fade_in as f32);
                    }
                }
                if !p.looped && p.fade_out > 0 {
                    let k = end - v.pos;
                    if k < p.fade_out {
                        g *= smooth(k as f32 / p.fade_out as f32);
                    }
                }
                if let Some((rem, total)) = &mut v.release {
                    if *rem == 0 {
                        break;
                    }
                    g *= smooth(*rem as f32 / *total as f32);
                    *rem -= 1;
                }
                let i = v.pos * 2;
                out[f * 2] += data[i] * g;
                out[f * 2 + 1] += data[i + 1] * g;
                v.pos += 1;
            }
        }
        // Voix terminées : position à zéro, son rendu à l'interface.
        let mut i = 0;
        while i < self.voices.len() {
            if matches!(self.voices[i].release, Some((0, _))) {
                let v = self.voices.swap_remove(i);
                if let Some(a) = pad_pos.get(v.slot as usize) {
                    a.store(0, Ordering::Relaxed);
                }
                self.dispose(v.sample);
            } else {
                i += 1;
            }
        }
        for v in &self.voices {
            if v.release.is_none()
                && let Some(a) = pad_pos.get(v.slot as usize)
            {
                a.store(v.pos as u64 + 1, Ordering::Relaxed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::bounded;

    fn sample(frames: usize) -> Arc<SampleData> {
        Arc::new(SampleData {
            sample_rate: 48000,
            frames,
            data: vec![0.5; frames * 2],
            peaks: Vec::new(),
        })
    }

    fn params(mode: PadMode, frames: usize) -> PadParams {
        PadParams {
            start: 0,
            end: frames,
            fade_in: 0,
            fade_out: 0,
            gain: 1.0,
            mode,
            looped: false,
        }
    }

    /// Charge : 64 voix (le maximum) en même temps, avec fondus et boucles, pour plusieurs
    /// tailles de buffer. Temps de calcul par bloc comparé au temps disponible.
    /// `cargo test --release -- --ignored --nocapture charge_soundboard`
    #[test]
    #[ignore]
    fn charge_soundboard_64_voix() {
        let sr = 48000usize;
        for block in [64usize, 256, 512, 1024] {
            let (tx, rx) = bounded(1024);
            let (gtx, _grx) = bounded(1024);
            let mut s = Sampler::new(rx, gtx, 0, sr as u32);
            let pos: Vec<AtomicU64> = (0..MAX_PADS).map(|_| AtomicU64::new(0)).collect();
            let long = sample(sr * 60);
            for slot in 0..MAX_VOICES as u16 {
                let mut p = params(PadMode::Toggle, sr * 60);
                p.fade_in = sr / 10;
                p.fade_out = sr / 10;
                p.looped = slot % 2 == 0;
                tx.send(PadCmd::Press {
                    slot,
                    sample: long.clone(),
                    params: p,
                })
                .unwrap();
            }
            let mut out = vec![0.0f32; block * 2];
            let budget = block as f64 / sr as f64;
            let (mut total, mut worst) = (0.0f64, 0.0f64);
            let blocks = (sr * 20) / block; // 20 s de son
            for _ in 0..blocks {
                out.fill(0.0);
                let t = std::time::Instant::now();
                s.render(&mut out, block, &pos);
                let d = t.elapsed().as_secs_f64();
                total += d;
                worst = worst.max(d);
            }
            assert_eq!(s.voices.len(), MAX_VOICES, "toutes les voix jouent");
            let avg = total / blocks as f64;
            println!(
                "soundboard 64 voix · bloc {block:>4} : moyenne {:>5.2} % du temps dispo, pire {:>5.2} %",
                avg / budget * 100.0,
                worst / budget * 100.0
            );
            assert!(avg < budget * 0.25, "trop lent en moyenne");
        }
    }

    #[test]
    fn oneshot_plays_to_end_then_stops() {
        let (tx, rx) = bounded(16);
        let (gtx, grx) = bounded(16);
        let mut s = Sampler::new(rx, gtx, 0, 48000);
        let pos: Vec<AtomicU64> = (0..4).map(|_| AtomicU64::new(0)).collect();
        tx.send(PadCmd::Press {
            slot: 1,
            sample: sample(100),
            params: params(PadMode::Toggle, 100),
        })
        .unwrap();
        let mut out = vec![0.0; 128];
        s.render(&mut out, 64, &pos);
        assert_eq!(out[0], 0.5);
        assert_eq!(pos[1].load(Ordering::Relaxed), 65);
        let mut out = vec![0.0; 128];
        s.render(&mut out, 64, &pos);
        assert_eq!(out[2 * 35], 0.5);
        assert_eq!(out[2 * 36], 0.0);
        assert_eq!(pos[1].load(Ordering::Relaxed), 0);
        assert!(
            grx.try_recv().is_ok(),
            "le son doit être rendu à l'interface"
        );
    }

    #[test]
    fn gate_stops_on_release() {
        let (tx, rx) = bounded(16);
        let (gtx, _grx) = bounded(16);
        let mut s = Sampler::new(rx, gtx, 0, 48000);
        let pos: Vec<AtomicU64> = (0..4).map(|_| AtomicU64::new(0)).collect();
        tx.send(PadCmd::Press {
            slot: 2,
            sample: sample(48000),
            params: params(PadMode::Gate, 48000),
        })
        .unwrap();
        let mut out = vec![0.0; 2048];
        s.render(&mut out, 1024, &pos);
        assert!(pos[2].load(Ordering::Relaxed) > 0);
        tx.send(PadCmd::Release { slot: 2 }).unwrap();
        for _ in 0..2 {
            out.fill(0.0);
            s.render(&mut out, 1024, &pos);
        }
        assert!(out.iter().all(|&x| x == 0.0));
        assert_eq!(pos[2].load(Ordering::Relaxed), 0);
    }

    /// Relancer : un second appui repart du début, l'ancien son s'efface sans clic.
    #[test]
    fn retrigger_restarts_from_start() {
        let (tx, rx) = bounded(16);
        let (gtx, _grx) = bounded(16);
        let mut s = Sampler::new(rx, gtx, 0, 48000);
        let pos: Vec<AtomicU64> = (0..4).map(|_| AtomicU64::new(0)).collect();
        let smp = sample(48000);
        let press = |tx: &crossbeam_channel::Sender<PadCmd>| {
            tx.send(PadCmd::Press {
                slot: 3,
                sample: smp.clone(),
                params: params(PadMode::Retrigger, 48000),
            })
            .unwrap();
        };
        press(&tx);
        let mut out = vec![0.0; 2048];
        for _ in 0..8 {
            s.render(&mut out, 1024, &pos);
        }
        assert!(pos[3].load(Ordering::Relaxed) > 8000);
        press(&tx);
        s.render(&mut out, 1024, &pos);
        // Reparti du début : position d'environ un bloc, pas 9 blocs.
        let p = pos[3].load(Ordering::Relaxed);
        assert!((1000..1100).contains(&p), "position {p}");
        // L'ancien son est en fondu puis disparaît : une seule voix reste ensuite.
        for _ in 0..2 {
            s.render(&mut out, 1024, &pos);
        }
        assert_eq!(s.voices.len(), 1);
        // Le son continue (pas d'arrêt comme en Lecture-Stop).
        assert!(out.iter().any(|&x| x != 0.0));
    }

    #[test]
    fn toggle_second_press_fades_out() {
        let (tx, rx) = bounded(16);
        let (gtx, _grx) = bounded(16);
        let mut s = Sampler::new(rx, gtx, 0, 48000);
        let pos: Vec<AtomicU64> = (0..4).map(|_| AtomicU64::new(0)).collect();
        let smp = sample(48000);
        tx.send(PadCmd::Press {
            slot: 0,
            sample: smp.clone(),
            params: params(PadMode::Toggle, 48000),
        })
        .unwrap();
        let mut out = vec![0.0; 2048];
        // Second appui 200 ms plus tard (au-delà de l'anti-rebond).
        for _ in 0..10 {
            s.render(&mut out, 1024, &pos);
        }
        tx.send(PadCmd::Press {
            slot: 0,
            sample: smp,
            params: params(PadMode::Toggle, 48000),
        })
        .unwrap();
        for _ in 0..4 {
            out.fill(0.0);
            s.render(&mut out, 1024, &pos);
        }
        assert!(out.iter().all(|&x| x == 0.0));
        assert_eq!(pos[0].load(Ordering::Relaxed), 0);
    }

    /// Rebond d'un bouton (second appui 20 ms après) : ignoré, le son continue.
    #[test]
    fn bouncing_press_is_ignored() {
        let (tx, rx) = bounded(16);
        let (gtx, _grx) = bounded(16);
        let mut s = Sampler::new(rx, gtx, 0, 48000);
        let pos: Vec<AtomicU64> = (0..4).map(|_| AtomicU64::new(0)).collect();
        let smp = sample(48000);
        let press = || PadCmd::Press {
            slot: 0,
            sample: smp.clone(),
            params: params(PadMode::Toggle, 48000),
        };
        tx.send(press()).unwrap();
        let mut out = vec![0.0; 2048];
        s.render(&mut out, 1024, &pos);
        tx.send(press()).unwrap();
        for _ in 0..4 {
            out.fill(0.0);
            s.render(&mut out, 1024, &pos);
        }
        assert!(out.iter().any(|&x| x != 0.0), "le son joue toujours");
    }

    /// Panique douce : fondu progressif, silence au bout du temps demandé.
    #[test]
    fn fade_all_fades_then_silences() {
        let (tx, rx) = bounded(16);
        let (gtx, _grx) = bounded(16);
        let mut s = Sampler::new(rx, gtx, 0, 48000);
        let pos: Vec<AtomicU64> = (0..4).map(|_| AtomicU64::new(0)).collect();
        tx.send(PadCmd::Press {
            slot: 0,
            sample: sample(96000),
            params: params(PadMode::Toggle, 96000),
        })
        .unwrap();
        let mut out = vec![0.0; 2048];
        s.render(&mut out, 1024, &pos);
        let level = |o: &[f32]| o.iter().fold(0f32, |m, x| m.max(x.abs()));
        let full = level(&out);
        tx.send(PadCmd::FadeAll { frames: 4800 }).unwrap();
        out.fill(0.0);
        s.render(&mut out, 1024, &pos);
        let mid = level(&out[1000..]);
        assert!(mid < full && mid > 0.0, "fondu en cours : {mid} / {full}");
        for _ in 0..6 {
            out.fill(0.0);
            s.render(&mut out, 1024, &pos);
        }
        assert!(out.iter().all(|&x| x == 0.0));
    }
}
