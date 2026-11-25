//! Tests de robustesse : fichiers abîmés, tronqués ou fabriqués (sessions, soundboards,
//! journaux de récupération, WAV). Aucun ne doit faire planter l'application : au pire
//! une erreur affichée ou du silence.
//!
//! Générateur pseudo-aléatoire maison (déterministe, sans dépendance) : un échec se
//! reproduit à l'identique. `TP_FUZZ_ITERS=20000 cargo test --release fuzz_` pour une
//! passe plus longue.

use crate::project::Project;
use crate::render::{TrackRenderer, WavSource};
use crate::soundboard::Board;
use std::path::PathBuf;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }
    /// Nombre « intéressant » : petit, limite d'un type, ou quelconque.
    fn edgy(&mut self) -> u64 {
        match self.below(8) {
            0 => 0,
            1 => 1,
            2 => u64::MAX - self.below(4),
            3 => u32::MAX as u64 + self.below(3) - 1,
            4 => i64::MAX as u64 + self.below(3) - 1,
            5 => self.below(100_000),
            6 => 1 << self.below(64),
            _ => self.next(),
        }
    }
}

fn iters(default: usize) -> usize {
    std::env::var("TP_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tp-fuzz-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// WAV valide de `frames` trames, puis mutations aléatoires d'octets / troncature.
fn wav_bytes(rng: &mut Rng, frames: u32) -> Vec<u8> {
    let channels = 1 + rng.below(2) as u16;
    let (bits, float) = match rng.below(4) {
        0 => (16, false),
        1 => (24, false),
        2 => (32, true),
        _ => (32, false),
    };
    let spec = hound::WavSpec {
        channels,
        sample_rate: 48000,
        bits_per_sample: bits,
        sample_format: if float {
            hound::SampleFormat::Float
        } else {
            hound::SampleFormat::Int
        },
    };
    let mut cur = std::io::Cursor::new(Vec::new());
    {
        let mut w = hound::WavWriter::new(&mut cur, spec).unwrap();
        for i in 0..frames * channels as u32 {
            if float {
                w.write_sample((i as f32 * 0.01).sin()).unwrap();
            } else {
                let max = (1i64 << (bits - 1)) - 1;
                w.write_sample(((i as i64 * 977) % max) as i32).unwrap();
            }
        }
        w.finalize().unwrap();
    }
    cur.into_inner()
}

fn mutate(rng: &mut Rng, mut b: Vec<u8>) -> Vec<u8> {
    match rng.below(4) {
        // Troncature (coupure de courant en pleine prise).
        0 => {
            let n = rng.below(b.len() as u64 + 1) as usize;
            b.truncate(n);
        }
        // Octets de l'en-tête modifiés.
        1 => {
            for _ in 0..1 + rng.below(6) {
                let i = rng.below(b.len().min(64) as u64) as usize;
                if i < b.len() {
                    b[i] = rng.next() as u8;
                }
            }
        }
        // Champs de taille / format remplacés par des valeurs extrêmes.
        2 => {
            for off in [4usize, 16, 20, 22, 24, 28, 32, 34, 40] {
                if rng.below(3) == 0 && off + 4 <= b.len() {
                    let v = rng.edgy() as u32;
                    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
                }
            }
        }
        // Octets n'importe où.
        _ => {
            for _ in 0..1 + rng.below(16) {
                let i = rng.below(b.len() as u64) as usize;
                if i < b.len() {
                    b[i] = rng.next() as u8;
                }
            }
        }
    }
    b
}

#[test]
fn fuzz_wav_readers_never_panic() {
    let dir = temp_dir("wav");
    let mut rng = Rng(0x7ac5_9105);
    for i in 0..iters(600) {
        let frames = rng.below(3000) as u32;
        let clean = wav_bytes(&mut rng, frames);
        let bytes = mutate(&mut rng, clean);
        let p = dir.join(format!("f{}.wav", i % 8));
        std::fs::write(&p, &bytes).unwrap();
        // Lecture séquentielle et repositionnements.
        if let Ok(mut src) = WavSource::open(&p) {
            let mut out = vec![0f32; 512 * 2];
            for _ in 0..4 {
                let ch = 1 + rng.below(2) as usize;
                src.seek(rng.edgy() % (src.frames() + 10));
                let n = src.read_frames(&mut out[..512 * ch], ch);
                assert!(n <= 512);
                assert!(
                    out[..n * ch].iter().all(|s| s.is_finite()),
                    "{:?} {:?}",
                    src.channels(),
                    &out[..8]
                );
            }
        }
        // Formes d'onde (avec leur cache).
        let peaks = std::sync::Arc::new(parking_lot::RwLock::new(Default::default()));
        crate::render::compute_peaks(&p, &peaks);
        // Réparation après crash : jamais de panique, longueur cohérente avec le fichier.
        if let Ok(frames) = crate::recovery::repair_wav(&p) {
            let len = std::fs::metadata(&p).unwrap().len();
            assert!(frames <= len, "{frames} trames pour {len} octets");
        }
    }
    std::fs::remove_dir_all(dir).ok();
}

/// Décodeur des pads et de l'import (symphonia) : toute panique interne doit rester
/// confinée — c'est le rôle de `decode_guarded` et du thread d'import.
#[test]
fn fuzz_sound_decoders_are_contained() {
    let dir = temp_dir("dec");
    let mut rng = Rng(0x51ed_270b);
    for i in 0..iters(250) {
        let frames = rng.below(5000) as u32;
        let clean = wav_bytes(&mut rng, frames);
        let bytes = mutate(&mut rng, clean);
        let p = dir.join(format!("s{}.wav", i % 4));
        std::fs::write(&p, &bytes).unwrap();
        let r = std::panic::catch_unwind(|| crate::soundboard::load_sample(&p, 48000));
        if let Ok(Ok(s)) = r {
            assert_eq!(s.data.len(), s.frames * 2);
            assert!(s.data.iter().all(|x| x.is_finite()));
        }
    }
    std::fs::remove_dir_all(dir).ok();
}

fn random_clip(rng: &mut Rng, id: u64) -> serde_json::Value {
    let files = [
        "a.wav",
        "../../x.wav",
        "",
        "C:\\\\x.wav",
        "absent.wav",
        "z.wav",
    ];
    serde_json::json!({
        "id": rng.edgy(),
        "file": files[rng.below(files.len() as u64) as usize],
        "start": rng.edgy(),
        "offset": rng.edgy(),
        "len": rng.edgy(),
        "src_len": rng.edgy(),
        "fade_in": rng.edgy(),
        "fade_out": rng.edgy(),
        "_n": id,
    })
}

fn random_session(rng: &mut Rng) -> String {
    let mut tracks = Vec::new();
    for t in 0..rng.below(5) {
        let clips: Vec<_> = (0..rng.below(5)).map(|c| random_clip(rng, c)).collect();
        tracks.push(serde_json::json!({
            "id": if rng.below(3) == 0 { t } else { rng.edgy() },
            "name": if rng.below(4) == 0 { String::new() } else { format!("P{t}") },
            "input": if rng.below(2) == 0 { serde_json::Value::Null } else { (rng.next() as u16).into() },
            "stereo": rng.below(2) == 0,
            "clips": clips,
        }));
    }
    let v = serde_json::json!({
        "name": "F",
        "sample_rate": match rng.below(4) { 0 => 0, 1 => u32::MAX as u64, 2 => 1, _ => 48000 },
        "tracks": tracks,
        "next_id": rng.edgy(),
    });
    let mut s = v.to_string();
    // Parfois un fichier tronqué ou abîmé (coupure pendant une écriture).
    if rng.below(5) == 0 && !s.is_empty() {
        let n = rng.below(s.len() as u64) as usize;
        s.truncate(s.floor_char_boundary(n));
    }
    s
}

/// Session fabriquée ou abîmée : le chargement réussit ou échoue proprement, et la session
/// chargée se manipule sans panique (rendu, coupe, suppression, sauvegarde).
#[test]
fn fuzz_session_files_never_panic() {
    let dir = temp_dir("sess");
    let audio = dir.join("Audio");
    std::fs::create_dir_all(&audio).unwrap();
    let mut rng = Rng(0x0dd_ba11);
    std::fs::write(audio.join("a.wav"), wav_bytes(&mut rng, 4000)).unwrap();
    std::fs::write(audio.join("z.wav"), b"RIFF\0\0\0\0WAVEjunk").unwrap();
    let path = dir.join("F.tracksplus");
    for _ in 0..iters(400) {
        std::fs::write(&path, random_session(&mut rng)).unwrap();
        let Ok(mut p) = Project::load(&path) else {
            continue;
        };
        exercise_project(&mut rng, &mut p);
    }
    std::fs::remove_dir_all(dir).ok();
}

fn exercise_project(rng: &mut Rng, p: &mut Project) {
    for c in p.tracks.iter().flat_map(|t| &t.clips) {
        assert!(!c.file.contains(['/', '\\']), "{}", c.file);
        assert!(!c.file.is_empty());
        assert!(c.fade_in.saturating_add(c.fade_out) <= c.len.max(1));
        let _ = c.end();
        let _ = c.fade_gain(rng.edgy());
    }
    let _ = p.audible();
    let _ = p.has_audio();
    for i in 0..p.tracks.len() {
        let mut r = TrackRenderer::new(p, i);
        let mut out = vec![0f32; 256 * r.channels()];
        for _ in 0..3 {
            r.render(rng.edgy() / 2, &mut out);
            assert!(out.iter().all(|s| s.is_finite()));
        }
        let _ = r.take_errors();
    }
    let at = rng.edgy();
    let _ = p.split_at(at, None, None);
    let (a, b) = (rng.edgy(), rng.edgy());
    let _ = p.delete_range(a.min(b), a.max(b), None);
    let _ = p.save();
}

/// Soundboard fabriqué ou abîmé : normalisé (grille, pages, pads) sans panique.
#[test]
fn fuzz_soundboard_files_never_panic() {
    let dir = temp_dir("board");
    let path = dir.join("soundboard.json");
    let mut rng = Rng(0xb0a2d);
    for _ in 0..iters(400) {
        let mut pages = Vec::new();
        for _ in 0..rng.below(4) {
            let pads: Vec<_> = (0..rng.below(80))
                .map(|_| {
                    if rng.below(3) == 0 {
                        serde_json::Value::Null
                    } else {
                        serde_json::json!({
                            "path": "x.wav",
                            "name": "p",
                            "color": rng.next() as u8,
                            "gain_db": f32::from_bits(rng.next() as u32) as f64,
                            "start": rng.edgy(),
                            "end": rng.edgy(),
                            "fade_in": rng.edgy(),
                            "fade_out": rng.edgy(),
                        })
                    }
                })
                .collect();
            pages.push(serde_json::json!({ "name": "P", "pads": pads }));
        }
        let mut s = serde_json::json!({
            "pages": pages,
            "cols": rng.edgy(),
            "rows": rng.edgy(),
        })
        .to_string();
        if rng.below(5) == 0 {
            let n = rng.below(s.len() as u64) as usize;
            s.truncate(s.floor_char_boundary(n));
        }
        std::fs::write(&path, s).unwrap();
        if let Ok(b) = Board::load(&path) {
            assert!(!b.pages.is_empty());
            for (slot, _) in b.all_pads() {
                assert!(b.pad(slot).is_some());
            }
        }
        let _ = Board::load_session(&dir);
    }
    std::fs::remove_dir_all(dir).ok();
}

/// Journal de récupération abîmé ou fabriqué : la session s'ouvre quand même.
#[test]
fn fuzz_recovery_journal_never_panics() {
    let dir = temp_dir("journal");
    let mut rng = Rng(0x10_93a1);
    let mut p = Project::new("J".into(), dir.clone(), 48000);
    p.ensure_dirs().unwrap();
    std::fs::write(p.audio_dir().join("t.wav"), wav_bytes(&mut rng, 2000)).unwrap();
    p.save().unwrap();
    for _ in 0..iters(300) {
        let takes: Vec<_> = (0..rng.below(4))
            .map(|_| {
                let file = ["t.wav", "../t.wav", "", "absent.wav"][rng.below(4) as usize];
                serde_json::json!({ "track_id": rng.edgy(), "file": file })
            })
            .collect();
        let mut s = serde_json::json!({
            "start": rng.edgy(),
            "play_offset": rng.edgy(),
            "latency": rng.edgy(),
            "takes": takes,
        })
        .to_string();
        if rng.below(4) == 0 {
            let n = rng.below(s.len() as u64) as usize;
            s.truncate(s.floor_char_boundary(n));
        }
        std::fs::write(crate::recovery::journal_path(&dir), s).unwrap();
        let mut q = Project::load(&p.file_path()).unwrap();
        let _ = crate::recovery::recover(&mut q);
        exercise_project(&mut rng, &mut q);
        // Session remise à zéro pour l'essai suivant.
        p.save().unwrap();
    }
    std::fs::remove_dir_all(dir).ok();
}

/// Mesure : calcul de la forme d'onde d'une prise de 10 min (24 bits mono, 48 kHz).
/// `cargo test --release -- --ignored --nocapture bench_peaks`
#[test]
#[ignore]
fn bench_peaks_10_min_take() {
    let dir = temp_dir("bench");
    let p = dir.join("long.wav");
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 48000,
        bits_per_sample: 24,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(&p, spec).unwrap();
    for i in 0..48000 * 600u32 {
        w.write_sample(((i % 4800) as i32 - 2400) * 1000).unwrap();
    }
    w.finalize().unwrap();
    let t = std::time::Instant::now();
    let peaks = std::sync::Arc::new(parking_lot::RwLock::new(Default::default()));
    crate::render::compute_peaks(&p, &peaks);
    let secs = t.elapsed().as_secs_f64();
    let mb = std::fs::metadata(&p).unwrap().len() as f64 / 1e6;
    println!(
        "forme d'onde : {mb:.0} Mo en {secs:.2} s ({:.0} Mo/s)",
        mb / secs
    );
    let t = std::time::Instant::now();
    let mut src = WavSource::open(&p).unwrap();
    let mut buf = vec![0f32; 4096 * 2];
    while src.read_frames(&mut buf, 2) > 0 {}
    let secs = t.elapsed().as_secs_f64();
    println!("lecture : {:.0}× le temps réel", 600.0 / secs);
    std::fs::remove_dir_all(dir).ok();
}
