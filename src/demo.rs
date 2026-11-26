//! Session de démonstration (captures d'écran du README), générée à la main :
//!
//!   cargo test --release -- --ignored make_demo_session --nocapture
//!
//! Dossier : `TP_DEMO_DIR`, sinon `Musique\TracksPlus\Démo TracksPlus`. Tout l'audio est
//! synthétisé (aucun enregistrement réel) ; la session est écrite par le code de
//! TracksPlus, donc toujours au bon format.

use crate::project::{Clip, Project};
use crate::soundboard::{BOARD_FILE, Board, Pad, Page, SOUNDS_DIR};
use std::f32::consts::TAU;
use std::path::{Path, PathBuf};

const SR: u32 = 48000;

/// Petit générateur pseudo-aléatoire (déterministe : même démo à chaque fois).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0
    }
}

fn hz(midi: f32) -> f32 {
    440.0 * 2f32.powf((midi - 69.0) / 12.0)
}

fn write_wav(path: &Path, channels: u16, data: &[f32]) {
    let spec = hound::WavSpec {
        channels,
        sample_rate: SR,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    for &x in data {
        w.write_sample(x.clamp(-1.0, 1.0)).unwrap();
    }
    w.finalize().unwrap();
}

/// Chant : phrases avec vibrato et attaques douces.
fn voice(secs: f32, notes: &[f32]) -> Vec<f32> {
    let n = (secs * SR as f32) as usize;
    let per = n / notes.len();
    let mut ph = 0f32;
    (0..n)
        .map(|i| {
            let k = (i / per).min(notes.len() - 1);
            let t = (i % per) as f32 / per as f32;
            let env =
                (t * 8.0).min(1.0) * (1.0 - t).powf(0.6) * if notes[k] > 0.0 { 1.0 } else { 0.0 };
            let vib = 1.0 + 0.006 * (i as f32 / SR as f32 * TAU * 5.5).sin();
            ph += hz(notes[k].max(1.0)) * vib / SR as f32;
            let s =
                (ph * TAU).sin() + 0.35 * (ph * 2.0 * TAU).sin() + 0.15 * (ph * 3.0 * TAU).sin();
            0.42 * env * s
        })
        .collect()
}

/// Guitare : accords pincés (Karplus-Strong).
fn guitar(secs: f32, chords: &[[f32; 3]], rng: &mut Rng) -> Vec<f32> {
    let n = (secs * SR as f32) as usize;
    let mut out = vec![0f32; n];
    let beat = SR as usize / 2;
    for (b, start) in (0..n).step_by(beat).enumerate() {
        let chord = chords[(b / 4) % chords.len()];
        for (s, &note) in chord.iter().enumerate() {
            let len = (SR as f32 / hz(note)) as usize;
            let mut buf: Vec<f32> = (0..len).map(|_| rng.next() * 0.5).collect();
            let off = start + s * 240;
            for (j, o) in out.iter_mut().skip(off).take(beat * 2).enumerate() {
                let a = buf[j % len];
                let c = buf[(j + 1) % len];
                buf[j % len] = 0.996 * 0.5 * (a + c);
                *o += 0.35 * a;
            }
        }
    }
    out
}

/// Basse : notes rondes.
fn bass(secs: f32, notes: &[f32]) -> Vec<f32> {
    let n = (secs * SR as f32) as usize;
    let beat = SR as usize;
    let mut ph = 0f32;
    (0..n)
        .map(|i| {
            let k = (i / beat) % notes.len();
            let t = (i % beat) as f32 / beat as f32;
            ph += hz(notes[k]) / SR as f32;
            let env = (t * 40.0).min(1.0) * (1.0 - t * 0.7);
            0.5 * env * ((ph * TAU).sin() + 0.2 * (ph * 2.0 * TAU).sin())
        })
        .collect()
}

/// Batterie stéréo : grosse caisse, caisse claire, charleston.
fn drums(secs: f32, rng: &mut Rng) -> Vec<f32> {
    let n = (secs * SR as f32) as usize;
    let mut out = vec![0f32; n * 2];
    let step = SR as usize / 4;
    for (k, start) in (0..n).step_by(step).enumerate() {
        let (kick, snare) = (k % 8 == 0 || k % 8 == 5, k % 8 == 4);
        for j in 0..step.min(n - start) {
            let t = j as f32 / SR as f32;
            let mut l = 0.0;
            let mut r = 0.0;
            if kick {
                let s =
                    0.9 * (-t * 18.0).exp() * (TAU * (50.0 + 90.0 * (-t * 30.0).exp()) * t).sin();
                l += s;
                r += s;
            }
            if snare {
                let s = 0.45 * (-t * 22.0).exp() * rng.next();
                l += s * 0.9;
                r += s;
            }
            let hat = 0.12 * (-t * 70.0).exp() * rng.next();
            l += hat * 0.6;
            r += hat;
            out[(start + j) * 2] += l;
            out[(start + j) * 2 + 1] += r;
        }
    }
    out
}

/// Claviers stéréo : nappes d'accords.
fn keys(secs: f32, chords: &[[f32; 3]]) -> Vec<f32> {
    let n = (secs * SR as f32) as usize;
    let bar = SR as usize * 2;
    let mut out = Vec::with_capacity(n * 2);
    for i in 0..n {
        let c = chords[(i / bar) % chords.len()];
        let t = i as f32 / SR as f32;
        let env = ((i % bar) as f32 / 4000.0).min(1.0);
        let mut l = 0.0;
        let mut r = 0.0;
        for (k, &note) in c.iter().enumerate() {
            let f = hz(note + 12.0);
            l += (TAU * f * t).sin() * if k == 2 { 0.5 } else { 1.0 };
            r += (TAU * f * 1.003 * t).sin() * if k == 0 { 0.5 } else { 1.0 };
        }
        out.push(0.12 * env * l);
        out.push(0.12 * env * r);
    }
    out
}

/// Son court de pad : bruit filtré, ton, ou glissando.
fn pad_sound(kind: u8, secs: f32, rng: &mut Rng) -> Vec<f32> {
    let n = (secs * SR as f32) as usize;
    let mut lp = 0f32;
    (0..n)
        .map(|i| {
            let t = i as f32 / SR as f32;
            let env = (t * 30.0).min(1.0) * (1.0 - t / secs).max(0.0).powf(1.5);
            let s = match kind {
                0 => {
                    // applaudissements : bruit en rafales
                    lp = 0.7 * lp + 0.3 * rng.next();
                    lp * (0.6 + 0.4 * (t * 37.0).sin().abs())
                }
                1 => (TAU * 880.0 * t).sin() * (-t * 3.0).exp(),
                2 => (TAU * (200.0 + 600.0 * t / secs) * t).sin(),
                _ => {
                    lp = 0.97 * lp + 0.03 * rng.next();
                    lp * 6.0
                }
            };
            0.6 * env * s
        })
        .collect()
}

fn demo_dir() -> PathBuf {
    std::env::var_os("TP_DEMO_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::audio_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join("TracksPlus")
                .join("Démo TracksPlus")
        })
}

#[test]
#[ignore]
fn make_demo_session() {
    let dir = demo_dir();
    let _ = std::fs::remove_dir_all(&dir);
    let mut p = Project::new("Démo TracksPlus".into(), dir.clone(), SR);
    p.ensure_dirs().unwrap();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let chords = [
        [57.0, 60.0, 64.0],
        [53.0, 57.0, 60.0],
        [48.0, 52.0, 55.0],
        [55.0, 59.0, 62.0],
    ];
    let audio = p.audio_dir();
    let mut clip_id = 1000;
    let mut add =
        |p: &mut Project, track: u64, file: &str, ch: u16, data: Vec<f32>, start_s: f32| {
            write_wav(&audio.join(file), ch, &data);
            let len = (data.len() / ch as usize) as u64;
            clip_id += 1;
            p.track_mut(track).unwrap().clips.push(Clip {
                id: clip_id,
                file: file.into(),
                start: (start_s * SR as f32) as u64,
                offset: 0,
                len,
                src_len: len,
                fade_in: 0,
                fade_out: 0,
            });
        };
    let chant = p.add_track(Some("Chant".into()), Some(0));
    let choeurs = p.add_track(Some("Chœurs".into()), Some(1));
    let guit = p.add_track(Some("Guitare".into()), Some(2));
    let basse = p.add_track(Some("Basse".into()), Some(3));
    let batt = p.add_track_ch(Some("Batterie".into()), Some(4), true);
    let clav = p.add_track_ch(Some("Claviers".into()), Some(6), true);
    let melody = [
        69.0, 71.0, 72.0, 0.0, 72.0, 74.0, 76.0, 74.0, 72.0, 0.0, 71.0, 69.0,
    ];
    add(&mut p, chant, "Chant_001.wav", 1, voice(12.0, &melody), 8.0);
    add(
        &mut p,
        chant,
        "Chant_002.wav",
        1,
        voice(14.0, &melody),
        24.0,
    );
    add(
        &mut p,
        choeurs,
        "Choeurs_001.wav",
        1,
        voice(14.0, &[64.0, 65.0, 67.0, 0.0, 67.0, 69.0, 71.0]),
        24.0,
    );
    add(
        &mut p,
        guit,
        "Guitare_001.wav",
        1,
        guitar(40.0, &chords, &mut rng),
        0.0,
    );
    add(
        &mut p,
        basse,
        "Basse_001.wav",
        1,
        bass(32.0, &[45.0, 41.0, 36.0, 43.0]),
        4.0,
    );
    add(
        &mut p,
        batt,
        "Batterie_001.wav",
        2,
        drums(36.0, &mut rng),
        4.0,
    );
    add(
        &mut p,
        clav,
        "Claviers_001.wav",
        2,
        keys(16.0, &chords),
        0.0,
    );
    add(
        &mut p,
        clav,
        "Claviers_002.wav",
        2,
        keys(14.0, &chords),
        24.0,
    );
    // Mix : panoramiques et niveaux d'un vrai mix.
    for (id, g, pan) in [
        (chant, 0.0, 0.0),
        (choeurs, -6.0, 0.25),
        (guit, -4.5, -0.45),
        (basse, -3.0, 0.0),
        (batt, -2.0, 0.0),
        (clav, -8.0, 0.3),
    ] {
        let t = p.track_mut(id).unwrap();
        t.gain_db = g;
        t.pan = pan;
        t.armed = id == chant || id == choeurs;
    }
    p.master_db = -3.0;
    for (s, name) in [
        (0.0, "Intro"),
        (8.0, "Couplet"),
        (24.0, "Refrain"),
        (38.0, "Final"),
    ] {
        let id = p.add_marker((s * SR as f32) as u64);
        if let Some(m) = p.markers.iter_mut().find(|m| m.id == id) {
            m.name = name.into();
        }
    }
    p.save().unwrap();

    // Soundboard : deux pages de pads nommés.
    let sounds = dir.join(SOUNDS_DIR);
    std::fs::create_dir_all(&sounds).unwrap();
    let mut b = Board {
        pages: vec![Page::new("Spectacle".into()), Page::new("Effets".into())],
        ..Default::default()
    };
    let pads: [(&str, u8, f32, usize, usize); 10] = [
        ("Applaudissements", 0, 6.0, 0, 0),
        ("Générique", 2, 8.0, 0, 1),
        ("Entracte", 1, 4.0, 0, 2),
        ("Rappel", 0, 5.0, 0, 3),
        ("Sonnerie", 1, 2.0, 0, 6),
        ("Transition", 2, 3.0, 0, 7),
        ("Vent", 3, 10.0, 0, 12),
        ("Orage", 3, 9.0, 0, 13),
        ("Cloche", 1, 3.0, 1, 0),
        ("Rires", 0, 4.0, 1, 1),
    ];
    for (k, (name, kind, secs, page, slot)) in pads.iter().enumerate() {
        let path = sounds.join(format!("{name}.wav"));
        write_wav(&path, 1, &pad_sound(*kind, *secs, &mut rng));
        let mut pad = Pad::new(path, (k % 10) as u8);
        pad.name = (*name).into();
        b.pages[*page].pads[*slot] = Some(pad);
    }
    b.save_session(&dir).unwrap();
    assert!(dir.join(BOARD_FILE).is_file());
    println!("Session de démo : {}", p.file_path().display());
}
