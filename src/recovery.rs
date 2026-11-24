//! Récupération après interruption (crash, coupure, fermeture forcée).
//!
//! Au lancement d'un enregistrement, un journal `.recording.json` est écrit dans le
//! dossier de la session (prises en cours, position). Il est supprimé à l'arrêt normal.
//! S'il est encore là à la réouverture, les prises sont réparées (en-tête WAV recalé
//! sur la taille réelle du fichier) et ajoutées à la session.

use crate::project::{Clip, Project};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const JOURNAL: &str = ".recording.json";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct JournalTake {
    pub track_id: u64,
    pub file: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Journal {
    pub start: u64,
    pub sample_rate: u32,
    pub takes: Vec<JournalTake>,
}

pub fn journal_path(dir: &Path) -> PathBuf {
    dir.join(JOURNAL)
}

pub fn write_journal(dir: &Path, j: &Journal) -> Result<()> {
    let p = journal_path(dir);
    let tmp = p.with_extension("tmp");
    crate::project::write_synced(&tmp, serde_json::to_string_pretty(j)?.as_bytes())?;
    std::fs::rename(tmp, p)?;
    Ok(())
}

pub fn remove_journal(dir: &Path) {
    let _ = std::fs::remove_file(journal_path(dir));
}

/// Recale les tailles RIFF / data d'un WAV interrompu sur la taille réelle du fichier.
/// Retourne le nombre de trames audio présentes.
pub fn repair_wav(path: &Path) -> Result<u64> {
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("Ouverture de {}", path.display()))?;
    let file_len = f.metadata()?.len();
    let mut head = [0u8; 12];
    f.read_exact(&mut head).context("Fichier trop court")?;
    if &head[0..4] != b"RIFF" || &head[8..12] != b"WAVE" {
        bail!("{} n'est pas un WAV", path.display());
    }
    let mut pos = 12u64;
    let mut block_align = 0u64;
    loop {
        if pos + 8 > file_len {
            bail!("Bloc audio introuvable dans {}", path.display());
        }
        f.seek(SeekFrom::Start(pos))?;
        let mut ch = [0u8; 8];
        f.read_exact(&mut ch)?;
        let size = u32::from_le_bytes([ch[4], ch[5], ch[6], ch[7]]) as u64;
        if &ch[0..4] == b"fmt " {
            let mut fmt = [0u8; 16];
            f.read_exact(&mut fmt)?;
            block_align = u16::from_le_bytes([fmt[12], fmt[13]]) as u64;
        } else if &ch[0..4] == b"data" {
            if block_align == 0 {
                bail!("En-tête fmt manquant");
            }
            let data_start = pos + 8;
            let avail = file_len.saturating_sub(data_start);
            let data_len = (avail / block_align * block_align).min(u32::MAX as u64 - 64);
            f.seek(SeekFrom::Start(pos + 4))?;
            f.write_all(&(data_len as u32).to_le_bytes())?;
            let riff = (data_start + data_len - 8).min(u32::MAX as u64);
            f.seek(SeekFrom::Start(4))?;
            f.write_all(&(riff as u32).to_le_bytes())?;
            f.sync_all()?;
            // Octets d'une trame incomplète : on les retire pour un fichier propre.
            if data_start + data_len < file_len {
                f.set_len(data_start + data_len)?;
            }
            return Ok(data_len / block_align);
        }
        pos += 8 + size + (size & 1);
    }
}

/// Applique un journal à une session : répare les fichiers et ajoute les prises
/// manquantes. Retourne le nombre de prises récupérées.
pub fn recover(project: &mut Project) -> Result<usize> {
    let jp = journal_path(&project.dir);
    let Ok(text) = std::fs::read_to_string(&jp) else {
        return Ok(0);
    };
    let j: Journal = serde_json::from_str(&text).context("Journal d'enregistrement illisible")?;
    let mut n = 0;
    for take in &j.takes {
        // Une prise longue peut s'étendre sur plusieurs fichiers (nom, nom_p2, nom_p3…).
        let mut pos = j.start;
        for k in 1.. {
            let file = crate::recorder::segment_name(&take.file, k);
            let path = project.audio_dir().join(&file);
            if !path.exists() {
                break;
            }
            // Réparation de l'en-tête ; sur un support en lecture seule, on lit la durée telle quelle.
            let frames = match repair_wav(&path) {
                Ok(f) => f,
                Err(_) => match hound::WavReader::open(&path) {
                    Ok(r) => r.duration() as u64,
                    Err(_) => break,
                },
            };
            if frames == 0 {
                continue;
            }
            let already = project
                .tracks
                .iter()
                .any(|t| t.clips.iter().any(|c| c.file == file));
            if already {
                // Prise déjà dans la session : on recale seulement sa longueur sur le fichier réel.
                for t in &mut project.tracks {
                    for c in t.clips.iter_mut().filter(|c| c.file == file) {
                        c.src_len = frames;
                        if c.offset + c.len > frames {
                            c.len = frames.saturating_sub(c.offset).max(1);
                            c.clamp_fades();
                        }
                    }
                }
            } else {
                let track_id = if project.tracks.iter().any(|t| t.id == take.track_id) {
                    take.track_id
                } else {
                    let name = take.file.trim_end_matches(".wav").to_string();
                    let stereo =
                        hound::WavReader::open(&path).is_ok_and(|r| r.spec().channels >= 2);
                    project.add_track_ch(Some(format!("{name} (récupérée)")), None, stereo)
                };
                let id = project.alloc_id();
                if let Some(t) = project.track_mut(track_id) {
                    t.clips.push(Clip {
                        id,
                        file: file.clone(),
                        start: pos,
                        offset: 0,
                        len: frames,
                        src_len: frames,
                        fade_in: 0,
                        fade_out: 0,
                    });
                    n += 1;
                }
            }
            pos += frames;
        }
    }
    project.dirty = true;
    project.save()?;
    remove_journal(&project.dir);
    Ok(n)
}

/// Mémorise la dernière session utilisée (pour la rouvrir après un crash).
pub fn remember_session(project: &Project) {
    if let Some(p) = state_path() {
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(p, project.file_path().to_string_lossy().as_bytes());
    }
}

/// Session à restaurer : la dernière utilisée, si un enregistrement y a été interrompu.
pub fn interrupted_session() -> Option<PathBuf> {
    let p = std::fs::read_to_string(state_path()?).ok()?;
    let path = PathBuf::from(p.trim());
    let dir = path.parent()?;
    (path.exists() && journal_path(dir).exists()).then_some(path)
}

fn state_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("TracksPlus").join("last_session"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_wav(path: &Path, frames: usize) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48000,
            bits_per_sample: 24,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for i in 0..frames {
            w.write_sample((i as i32 % 1000) * 100).unwrap();
        }
        w.finalize().unwrap();
    }

    #[test]
    fn repair_interrupted_wav() {
        let dir = std::env::temp_dir().join(format!("tp-rec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.wav");
        write_wav(&path, 1000);
        // Simule un crash : 500 trames écrites après la dernière mise à jour de l'en-tête,
        // plus un octet d'une trame incomplète.
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(&vec![1u8; 500 * 3 + 1]).unwrap();
        drop(f);
        assert_eq!(hound::WavReader::open(&path).unwrap().duration(), 1000);
        assert_eq!(repair_wav(&path).unwrap(), 1500);
        assert_eq!(hound::WavReader::open(&path).unwrap().duration(), 1500);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn recover_adds_missing_takes() {
        let dir = std::env::temp_dir().join(format!("tp-recover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut p = Project::new("R".into(), dir.clone(), 48000);
        p.ensure_dirs().unwrap();
        let t = p.add_track(Some("Voix".into()), Some(0));
        p.save().unwrap();
        write_wav(&p.audio_dir().join("Voix_001.wav"), 4800);
        write_journal(
            &dir,
            &Journal {
                start: 960,
                sample_rate: 48000,
                takes: vec![
                    JournalTake {
                        track_id: t,
                        file: "Voix_001.wav".into(),
                    },
                    JournalTake {
                        track_id: 999,
                        file: "Absent.wav".into(),
                    },
                ],
            },
        )
        .unwrap();
        let mut q = Project::load(&p.file_path()).unwrap();
        assert_eq!(recover(&mut q).unwrap(), 1);
        let c = &q.tracks[0].clips[0];
        assert_eq!((c.start, c.len), (960, 4800));
        assert!(!journal_path(&dir).exists());
        // Rejouer la récupération ne duplique rien.
        assert_eq!(recover(&mut q).unwrap(), 0);
        std::fs::remove_dir_all(dir).ok();
    }
}
