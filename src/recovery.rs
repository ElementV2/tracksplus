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
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub const JOURNAL: &str = ".recording.json";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct JournalTake {
    pub track_id: u64,
    pub file: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Journal {
    pub start: u64,
    /// Prise faite en écoutant : position de lecture au début réel de la prise (trames).
    #[serde(default)]
    pub play_offset: u64,
    /// Latence entrée + sortie à retirer au début de la prise (trames).
    #[serde(default)]
    pub latency: u64,
    pub sample_rate: u32,
    pub takes: Vec<JournalTake>,
    /// Prises précédentes pas encore terminées (leur disque ne répondait plus quand
    /// celle-ci a commencé) : récupérées elles aussi, chacune avec sa position.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra: Vec<Journal>,
}

impl Journal {
    /// Ce journal et ceux qu'il contient (`extra`), à plat (profondeur et nombre bornés).
    fn groups(&self) -> Vec<&Journal> {
        let mut out = vec![self];
        let mut i = 0;
        while i < out.len() && out.len() < 64 {
            let g = out[i];
            out.extend(g.extra.iter().take(64 - out.len()));
            i += 1;
        }
        out
    }

    /// Prises de ce journal (le sien et ceux qu'il porte), hors celles de `live`, chacune
    /// seule (sans les prises en attente qu'elle portait) : à reporter dans un autre journal.
    pub fn pending_groups(&self, live: &[Journal]) -> Vec<Journal> {
        self.groups()
            .into_iter()
            .filter(|g| !g.takes.is_empty() && !live.iter().any(|l| l.same_takes(g)))
            .map(|g| Journal {
                extra: Vec::new(),
                ..g.clone()
            })
            .collect()
    }

    /// Mêmes fichiers de prise.
    fn same_takes(&self, other: &Journal) -> bool {
        self.takes.len() == other.takes.len()
            && self
                .takes
                .iter()
                .zip(&other.takes)
                .all(|(a, b)| a.file == b.file)
    }
}

pub fn journal_path(dir: &Path) -> PathBuf {
    dir.join(JOURNAL)
}

/// Garde `j` dans le journal de `dir` sans rien perdre de ce qui y est déjà : ajouté aux
/// prises en attente du journal existant (sauf s'il y figure déjà), ou écrit tel quel.
pub fn keep_journal(dir: &Path, j: &Journal) -> Result<()> {
    let existing = std::fs::read_to_string(journal_path(dir))
        .ok()
        .and_then(|t| serde_json::from_str::<Journal>(&t).ok());
    match existing {
        Some(mut cur) => {
            if cur.groups().iter().any(|g| g.same_takes(j)) {
                return Ok(());
            }
            cur.extra.push(j.clone());
            write_journal(dir, &cur)
        }
        None => write_journal(dir, j),
    }
}

/// Rang des écritures de journal : chaque écriture (ou suppression) reçoit son rang au
/// moment où elle est décidée, sur le thread qui la décide. Un journal n'est jamais
/// remplacé par un plus ancien, même écrit plus tard par un thread d'écriture resté
/// bloqué sur un disque lent (il ne listerait pas les fichiers de la prise suivante).
static JOURNAL_SEQ: AtomicU64 = AtomicU64::new(1);

/// Rang d'une écriture de journal décidée maintenant (voir `write_journal_ranked`).
pub fn next_journal_rank() -> u64 {
    JOURNAL_SEQ.fetch_add(1, Ordering::Relaxed)
}

/// Dernier rang écrit pour chaque dossier (un verrou par dossier : un disque bloqué ne
/// retient que les journaux de ce disque).
fn rank_slot(dir: &Path) -> Arc<parking_lot::Mutex<u64>> {
    static SLOTS: parking_lot::Mutex<Vec<(PathBuf, Arc<parking_lot::Mutex<u64>>)>> =
        parking_lot::Mutex::new(Vec::new());
    let mut v = SLOTS.lock();
    if let Some((_, s)) = v.iter().find(|(d, _)| d == dir) {
        return s.clone();
    }
    let s = Arc::new(parking_lot::Mutex::new(0));
    v.push((dir.to_path_buf(), s.clone()));
    s
}

/// Écriture atomique du journal de `dir` au rang `rank` : fichier temporaire synchronisé,
/// puis remplacement, seulement si aucun journal de rang plus récent n'a été écrit (ou
/// supprimé) entre-temps. `Ok(false)` : écriture abandonnée car périmée.
fn write_ranked(dir: &Path, j: &Journal, rank: u64) -> Result<bool> {
    let path = journal_path(dir);
    if *rank_slot(dir).lock() > rank {
        return Ok(false);
    }
    let tmp = crate::project::temp_sibling(&path, "tmp");
    if let Err(e) = crate::project::write_synced(&tmp, serde_json::to_string_pretty(j)?.as_bytes())
    {
        let _ = std::fs::remove_file(&tmp);
        return Err(anyhow::Error::new(e).context(format!("Écriture de {}", tmp.display())));
    }
    // Vérifié de nouveau après l'écriture forcée (la partie lente) : remplacement sous le
    // verrou du dossier, jamais par-dessus un journal plus récent.
    let slot = rank_slot(dir);
    let mut last = slot.lock();
    if *last > rank {
        let _ = std::fs::remove_file(&tmp);
        return Ok(false);
    }
    if let Err(e) = crate::project::replace_file(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(anyhow::Error::new(e).context(format!("Écriture de {}", path.display())));
    }
    *last = rank;
    Ok(true)
}

/// Écrit le journal de la prise en cours (forcé sur le disque) et note la session parmi
/// celles dont un enregistrement n'est pas terminé (voir `interrupted_sessions`).
pub fn write_journal(dir: &Path, j: &Journal) -> Result<()> {
    write_journal_ranked(dir, j, next_journal_rank())
}

/// Comme `write_journal`, au rang `rank` obtenu plus tôt (`next_journal_rank`) : ignorée si
/// un journal plus récent a déjà été écrit pour ce dossier.
pub fn write_journal_ranked(dir: &Path, j: &Journal, rank: u64) -> Result<()> {
    if write_ranked(dir, j, rank)? {
        track_pending(dir, true);
    }
    Ok(())
}

/// Journal du double de la session (copie de sécurité) : jamais proposé à la
/// récupération automatique au lancement (c'est la session principale qui l'est).
pub fn write_journal_untracked(dir: &Path, j: &Journal) -> Result<()> {
    write_ranked(dir, j, next_journal_rank()).map(|_| ())
}

pub fn remove_journal(dir: &Path) {
    let rank = next_journal_rank();
    let slot = rank_slot(dir);
    let mut last = slot.lock();
    if *last > rank {
        return;
    }
    let _ = std::fs::remove_file(journal_path(dir));
    *last = rank;
    drop(last);
    track_pending(dir, false);
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

/// Résultat d'une récupération : prises réintégrées, et sauvegarde de la session (en cas
/// d'échec, les prises sont dans la session en mémoire et le journal est conservé).
#[derive(Debug)]
pub struct Recovered {
    pub takes: usize,
    pub saved: Result<()>,
}

/// Fichiers d'une prise présents dans `names` (noms du dossier Audio) : le fichier de base
/// et ses suites `_pN`, dans l'ordre. Les numéros manquants (fichier vide supprimé) ne
/// coupent pas la liste.
fn take_segments(base: &str, names: &[String]) -> Vec<String> {
    let stem = base.trim_end_matches(".wav").to_lowercase();
    let base_low = base.to_lowercase();
    let mut found: Vec<(usize, &String)> = names
        .iter()
        .filter_map(|n| {
            let low = n.to_lowercase();
            if low == base_low {
                return Some((1, n));
            }
            let k = low
                .strip_prefix(&stem)?
                .strip_prefix("_p")?
                .strip_suffix(".wav")?;
            // Chiffres seulement (« _p2 », pas « _p2b » ni « _p+2 »).
            if k.is_empty() || !k.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let k: usize = k.parse().ok()?;
            (k >= 2).then_some((k, n))
        })
        .collect();
    found.sort();
    found.dedup_by_key(|(k, _)| *k);
    found.into_iter().map(|(_, n)| n.clone()).collect()
}

/// Applique le journal de la session : répare les fichiers et ajoute les prises
/// manquantes, puis sauvegarde. `Err` seulement si le journal est illisible (fichiers
/// laissés dans Audio/).
#[cfg(test)]
pub fn recover(project: &mut Project) -> Result<Recovered> {
    recover_with(project, &[])
}

/// Comme `recover`, sauf pour les prises de `live`, encore en cours d'écriture (prise
/// restée bloquée sur un disque lent) : leurs fichiers ne sont pas touchés (jamais de
/// réparation d'un fichier qu'un thread d'écriture complète encore), elles restent dans
/// le journal et seront intégrées à la fin de leur écriture.
pub fn recover_with(project: &mut Project, live: &[Journal]) -> Result<Recovered> {
    let jp = journal_path(&project.dir);
    let Ok(text) = std::fs::read_to_string(&jp) else {
        return Ok(Recovered {
            takes: 0,
            saved: Ok(()),
        });
    };
    let journal: Journal =
        serde_json::from_str(&text).context("Journal d'enregistrement illisible")?;
    let (n, sidecars) = apply_journal(project, &journal, live);
    project.dirty = true;
    let saved = project.save();
    if saved.is_ok() {
        // Positions notées supprimées seulement maintenant : si la sauvegarde échoue, le
        // journal reste et une récupération ultérieure en a encore besoin.
        for p in sidecars {
            let _ = std::fs::remove_file(p);
        }
        if live.is_empty() {
            remove_journal(&project.dir);
        } else {
            let rest = Journal {
                start: 0,
                play_offset: 0,
                latency: 0,
                sample_rate: journal.sample_rate,
                takes: Vec::new(),
                extra: live.to_vec(),
            };
            if let Err(e) = write_journal(&project.dir, &rest) {
                eprintln!("Récupération : journal des prises en cours non écrit ({e:#})");
            }
        }
    }
    Ok(Recovered { takes: n, saved })
}

/// Réintègre les prises d'un journal dans `project`, en mémoire (sans sauvegarde) :
/// fichiers réparés, prises manquantes ajoutées, prises déjà présentes recalées sur la
/// longueur réelle de leur fichier. Les prises de `live` (encore en cours d'écriture) sont
/// laissées de côté. Rend le nombre de prises ajoutées et les positions notées (`.start`)
/// utilisées, à supprimer une fois la session sauvegardée.
pub fn apply_journal(
    project: &mut Project,
    journal: &Journal,
    live: &[Journal],
) -> (usize, Vec<PathBuf>) {
    let groups: Vec<&Journal> = journal
        .groups()
        .into_iter()
        .filter(|g| !g.takes.is_empty() && !live.iter().any(|l| l.same_takes(g)))
        .collect();
    let mut sidecars = Vec::new();
    if groups.is_empty() {
        return (0, sidecars);
    }
    let names: Vec<String> = std::fs::read_dir(project.audio_dir())
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    let mut n = 0;
    // Prise interrompue, et les précédentes restées en attente (chacune à sa position).
    let all = groups
        .into_iter()
        .flat_map(|g| g.takes.iter().map(move |t| (g, t)));
    for (j, take) in all {
        // Journal falsifié ou abîmé : jamais de chemin hors du dossier Audio/.
        let base = crate::project::safe_file_name(&take.file);
        if base.is_empty() {
            continue;
        }
        // Une prise longue peut s'étendre sur plusieurs fichiers (nom, nom_p2, nom_p3…).
        // Fichiers supposés bout à bout, sauf ceux repris après une erreur d'écriture (leur
        // position est notée à côté), décalés de la latence comme une fin de prise normale.
        let origin = j
            .start
            .saturating_add(j.play_offset)
            .min(crate::project::MAX_POS);
        let mut seg_start = 0u64;
        for file in take_segments(&base, &names) {
            let path = project.audio_dir().join(&file);
            // Réparation de l'en-tête ; sur un support en lecture seule, on lit la durée
            // telle quelle. Fichier illisible (vide, abîmé) : on passe au suivant, sans
            // abandonner ceux d'après.
            let frames = match repair_wav(&path) {
                Ok(f) => f,
                Err(_) => match hound::WavReader::open(&path) {
                    Ok(r) => r.duration() as u64,
                    Err(e) => {
                        eprintln!("Récupération : {} ignoré ({e})", path.display());
                        continue;
                    }
                },
            };
            // Fichier repris après une erreur d'écriture : sa vraie position est notée.
            let side = project
                .audio_dir()
                .join(crate::recorder::start_sidecar(&file));
            if let Some(s) = std::fs::read_to_string(&side)
                .ok()
                .and_then(|t| t.trim().parse::<u64>().ok())
            {
                seg_start = s.min(crate::project::MAX_POS);
            }
            sidecars.push(side);
            if frames == 0 {
                continue;
            }
            // Même recalage qu'à la fin normale d'une prise : début du premier fichier
            // sauté de la latence (vaut aussi pour placer les fichiers suivants).
            let lat = j.latency.min(crate::project::MAX_POS);
            // Morceau entièrement avant la latence (coupé par une erreur) : rien à poser.
            if seg_start > 0 && seg_start + frames <= lat {
                seg_start += frames;
                continue;
            }
            let skip = lat
                .saturating_sub(seg_start)
                .min(frames.saturating_sub(crate::project::MIN_CLIP));
            let pos = (origin + seg_start + skip).saturating_sub(lat).max(origin);
            let already = project
                .tracks
                .iter()
                .any(|t| t.clips.iter().any(|c| c.file == file));
            if already {
                // Prise déjà dans la session : on recale seulement sa longueur sur le fichier
                // réel (prolongée si elle allait jusqu'au bout du fichier, réintégré plus tôt
                // alors que son écriture n'était pas finie).
                for t in &mut project.tracks {
                    for c in t.clips.iter_mut().filter(|c| c.file == file) {
                        let to_end = c.offset + c.len >= c.src_len;
                        c.src_len = frames;
                        if to_end && frames > c.offset {
                            c.len = frames - c.offset;
                            c.clamp_fades();
                        } else if c.offset + c.len > frames {
                            c.len = frames.saturating_sub(c.offset).max(1);
                            c.clamp_fades();
                        }
                    }
                }
            } else {
                let track_id = if project.tracks.iter().any(|t| t.id == take.track_id) {
                    take.track_id
                } else {
                    let name = base.trim_end_matches(".wav").to_string();
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
                        offset: skip,
                        len: frames - skip,
                        src_len: frames,
                        fade_in: 0,
                        fade_out: 0,
                    });
                    n += 1;
                }
            }
            seg_start += frames;
        }
    }
    (n, sidecars)
}

/// Mémorise la dernière session utilisée (pour la rouvrir après un crash).
pub fn remember_session(project: &Project) {
    remember_session_path(&project.file_path());
}

/// Comme `remember_session`, depuis n'importe quel thread (début de prise : jamais
/// d'écriture forcée sur le disque dans le thread de l'interface avant la capture).
pub fn remember_session_path(path: &Path) {
    static LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
    let _g = LOCK.lock();
    if let Some(p) = state_path() {
        let _ = crate::project::write_atomic(&p, path.to_string_lossy().as_bytes());
    }
    crate::sessions::remember(path);
}

/// Dernière session utilisée, si elle existe encore.
pub fn last_session() -> Option<PathBuf> {
    let p = std::fs::read_to_string(state_path()?).ok()?;
    let path = PathBuf::from(p.trim());
    path.exists().then_some(path)
}

/// Sessions dont un enregistrement n'a pas été terminé, celle à restaurer en premier : la
/// dernière utilisée si un enregistrement y a été interrompu, puis toute autre session
/// restée en suspens (prise bloquée, puis une autre session ouverte avant de quitter).
pub fn interrupted_sessions() -> Vec<PathBuf> {
    let last = state_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|p| PathBuf::from(p.trim()));
    let mut out = Vec::new();
    if let Some(path) = &last
        && let Some(dir) = path.parent()
        && path.exists()
        && journal_path(dir).exists()
    {
        out.push(path.clone());
    }
    let list = pending_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default();
    for d in pending_dirs(&list) {
        if !journal_path(&d).exists() || out.iter().any(|p| p.parent() == Some(d.as_path())) {
            continue;
        }
        if let Some(f) = session_file_in(&d) {
            out.push(f);
        }
    }
    out
}

/// Fichier de session d'un dossier de session.
fn session_file_in(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.is_file()
                && p.extension()
                    .is_some_and(|x| x.eq_ignore_ascii_case(crate::project::PROJECT_EXT))
        })
}

fn state_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("TracksPlus").join("last_session"))
}

/// Liste des sessions dont un enregistrement n'est pas terminé (une par ligne). Jamais
/// dans les tests (le dossier de réglages de l'utilisateur n'est pas touché).
fn pending_path() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    dirs::config_dir().map(|d| d.join("TracksPlus").join("pending_recordings"))
}

/// Dossiers de la liste (lignes non vides), au plus 64.
fn pending_dirs(list: &str) -> Vec<PathBuf> {
    list.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(64)
        .map(PathBuf::from)
        .collect()
}

/// Nouvelle liste avec `dir` ajouté (`add`, en tête) ou retiré ; `None` si rien ne change.
fn pending_with(list: &str, dir: &Path, add: bool) -> Option<String> {
    let mut dirs = pending_dirs(list);
    let had = dirs.iter().any(|d| d == dir);
    if had == add {
        return None;
    }
    dirs.retain(|d| d != dir);
    if add {
        dirs.insert(0, dir.to_path_buf());
        dirs.truncate(64);
    }
    let mut out = String::new();
    for d in dirs {
        out.push_str(&d.to_string_lossy());
        out.push('\n');
    }
    Some(out)
}

/// Tient à jour la liste des sessions à récupérer (erreurs ignorées : ce n'est qu'une aide
/// au lancement, le journal lui-même reste dans le dossier de la session).
fn track_pending(dir: &Path, add: bool) {
    static LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
    let Some(p) = pending_path() else {
        return;
    };
    let _g = LOCK.lock();
    let list = std::fs::read_to_string(&p).unwrap_or_default();
    if let Some(new) = pending_with(&list, dir, add) {
        let _ = crate::project::write_atomic(&p, new.as_bytes());
    }
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
                play_offset: 240,
                latency: 100,
                sample_rate: 48000,
                extra: Vec::new(),
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
        assert_eq!(recover(&mut q).unwrap().takes, 1);
        let c = &q.tracks[0].clips[0];
        // Recalée comme une fin de prise normale : décalage de lecture et latence.
        assert_eq!((c.start, c.offset, c.len), (960 + 240, 100, 4700));
        assert!(!journal_path(&dir).exists());
        // Rejouer la récupération ne duplique rien.
        assert_eq!(recover(&mut q).unwrap().takes, 0);
        std::fs::remove_dir_all(dir).ok();
    }

    /// Prise coupée par une erreur d'écriture puis reprise : après un crash, le fichier
    /// repris retrouve sa vraie place grâce à sa position notée à côté.
    #[test]
    fn recover_places_resumed_file_after_gap() {
        let dir = std::env::temp_dir().join(format!("tp-recover-gap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut p = Project::new("G".into(), dir.clone(), 48000);
        p.ensure_dirs().unwrap();
        let t = p.add_track(Some("Voix".into()), Some(0));
        p.save().unwrap();
        write_wav(&p.audio_dir().join("Voix_001.wav"), 4800);
        // 1000 trames perdues, puis reprise à la trame 5800.
        write_wav(&p.audio_dir().join("Voix_001_p2.wav"), 2000);
        std::fs::write(
            p.audio_dir()
                .join(crate::recorder::start_sidecar("Voix_001_p2.wav")),
            "5800",
        )
        .unwrap();
        write_journal(
            &dir,
            &Journal {
                start: 0,
                play_offset: 0,
                latency: 100,
                sample_rate: 48000,
                extra: Vec::new(),
                takes: vec![JournalTake {
                    track_id: t,
                    file: "Voix_001.wav".into(),
                }],
            },
        )
        .unwrap();
        let mut q = Project::load(&p.file_path()).unwrap();
        assert_eq!(recover(&mut q).unwrap().takes, 2);
        let c: Vec<_> = q.tracks[0]
            .clips
            .iter()
            .map(|c| (c.start, c.offset, c.len))
            .collect();
        assert_eq!(c, [(0, 100, 4700), (5800 - 100, 0, 2000)]);
        assert!(
            !p.audio_dir()
                .join(crate::recorder::start_sidecar("Voix_001_p2.wav"))
                .exists()
        );
        std::fs::remove_dir_all(dir).ok();
    }

    /// Suites d'une prise retrouvées dans le dossier, dans l'ordre, trous de numérotation
    /// compris, sans confondre une autre prise au nom voisin.
    #[test]
    fn take_segments_lists_all_parts_in_order() {
        let names: Vec<String> = [
            "Voix_001_p10.wav",
            "Voix_001.wav",
            "voix_001_p3.wav",
            "Voix_001_p3.wav.start",
            "Voix_0012.wav",
            "Voix_001_p2b.wav",
            "Voix_001_p1.wav",
            "Basse_001_p2.wav",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(
            take_segments("Voix_001.wav", &names),
            ["Voix_001.wav", "voix_001_p3.wav", "Voix_001_p10.wav"]
        );
        assert!(take_segments("Absent.wav", &names).is_empty());
    }

    /// Crash après un segment vide (disque plein) puis une reprise : la reprise est
    /// réintégrée quand même, à sa place ; une sauvegarde impossible n'efface pas le compte.
    #[test]
    fn recover_skips_empty_segment_and_keeps_later_ones() {
        let dir = std::env::temp_dir().join(format!("tp-recover-hole-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut p = Project::new("H".into(), dir.clone(), 48000);
        p.ensure_dirs().unwrap();
        let t = p.add_track(Some("Voix".into()), Some(0));
        p.save().unwrap();
        write_wav(&p.audio_dir().join("Voix_001.wav"), 4800);
        // _p2 : créé, rien d'écrit (0 octet) ; _p3 manquant (supprimé) ; _p4 repris.
        std::fs::write(p.audio_dir().join("Voix_001_p2.wav"), b"").unwrap();
        write_wav(&p.audio_dir().join("Voix_001_p4.wav"), 1000);
        std::fs::write(
            p.audio_dir()
                .join(crate::recorder::start_sidecar("Voix_001_p4.wav")),
            "9000",
        )
        .unwrap();
        let j = Journal {
            start: 0,
            play_offset: 0,
            latency: 0,
            sample_rate: 48000,
            extra: Vec::new(),
            takes: vec![JournalTake {
                track_id: t,
                file: "Voix_001.wav".into(),
            }],
        };
        write_journal(&dir, &j).unwrap();
        let mut q = Project::load(&p.file_path()).unwrap();
        let r = recover(&mut q).unwrap();
        assert_eq!(r.takes, 2);
        assert!(r.saved.is_ok());
        let c: Vec<_> = q.tracks[0]
            .clips
            .iter()
            .map(|c| (c.file.as_str(), c.start, c.len))
            .collect();
        assert_eq!(
            c,
            [("Voix_001.wav", 0, 4800), ("Voix_001_p4.wav", 9000, 1000)]
        );
        std::fs::remove_dir_all(dir).ok();
    }

    /// Prise précédente restée en attente (disque bloqué) quand une nouvelle a commencé :
    /// un seul journal les porte toutes les deux, chacune récupérée à sa position ; une
    /// prise réintégrée trop tôt (fichier pas fini) est prolongée à sa vraie longueur.
    #[test]
    fn recover_journal_with_pending_previous_take() {
        let dir = std::env::temp_dir().join(format!("tp-recover-extra-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut p = Project::new("X".into(), dir.clone(), 48000);
        p.ensure_dirs().unwrap();
        let t = p.add_track(Some("Voix".into()), Some(0));
        // Prise précédente réintégrée pendant le blocage avec 1000 trames seulement.
        p.track_mut(t).unwrap().clips.push(Clip {
            id: 50,
            file: "Voix_001.wav".into(),
            start: 100,
            offset: 0,
            len: 1000,
            src_len: 1000,
            fade_in: 0,
            fade_out: 0,
        });
        p.next_id = 60;
        p.save().unwrap();
        write_wav(&p.audio_dir().join("Voix_001.wav"), 3000);
        write_wav(&p.audio_dir().join("Voix_002.wav"), 500);
        let take = |file: &str| JournalTake {
            track_id: t,
            file: file.into(),
        };
        let old = Journal {
            start: 100,
            play_offset: 0,
            latency: 0,
            sample_rate: 48000,
            takes: vec![take("Voix_001.wav")],
            extra: Vec::new(),
        };
        let new = Journal {
            start: 8000,
            play_offset: 0,
            latency: 0,
            sample_rate: 48000,
            takes: vec![take("Voix_002.wav")],
            extra: Vec::new(),
        };
        write_journal(&dir, &new).unwrap();
        keep_journal(&dir, &old).unwrap();
        // Déjà présent : rien n'est dupliqué.
        keep_journal(&dir, &old).unwrap();
        let text = std::fs::read_to_string(journal_path(&dir)).unwrap();
        let j: Journal = serde_json::from_str(&text).unwrap();
        assert_eq!(j.extra.len(), 1);
        let mut q = Project::load(&p.file_path()).unwrap();
        let r = recover(&mut q).unwrap();
        assert_eq!(r.takes, 1, "seule la nouvelle prise est ajoutée");
        let mut c: Vec<_> = q.tracks[0]
            .clips
            .iter()
            .map(|c| (c.file.as_str(), c.start, c.len))
            .collect();
        c.sort();
        assert_eq!(
            c,
            [("Voix_001.wav", 100, 3000), ("Voix_002.wav", 8000, 500)]
        );
        std::fs::remove_dir_all(dir).ok();
    }

    /// Session rouverte pendant qu'une prise bloquée écrit encore : ses fichiers ne sont ni
    /// réparés ni tronqués (le thread d'écriture garde sa position), elle reste dans le
    /// journal ; les autres prises du journal sont récupérées.
    #[test]
    fn recover_never_touches_takes_still_writing() {
        let dir = std::env::temp_dir().join(format!("tp-recover-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut p = Project::new("L".into(), dir.clone(), 48000);
        p.ensure_dirs().unwrap();
        let t = p.add_track(Some("Voix".into()), Some(0));
        p.save().unwrap();
        write_wav(&p.audio_dir().join("Voix_001.wav"), 1000);
        let live_path = p.audio_dir().join("Voix_002.wav");
        write_wav(&live_path, 1000);
        // En cours d'écriture : une trame incomplète en fin de fichier.
        let mut f = OpenOptions::new().append(true).open(&live_path).unwrap();
        f.write_all(&[1u8; 3 * 10 + 2]).unwrap();
        drop(f);
        let before = std::fs::read(&live_path).unwrap();
        let take = |file: &str, start| Journal {
            start,
            play_offset: 0,
            latency: 0,
            sample_rate: 48000,
            takes: vec![JournalTake {
                track_id: t,
                file: file.into(),
            }],
            extra: Vec::new(),
        };
        let live = take("Voix_002.wav", 5000);
        let mut j = take("Voix_001.wav", 0);
        j.extra.push(live.clone());
        write_journal(&dir, &j).unwrap();
        let mut q = Project::load(&p.file_path()).unwrap();
        let r = recover_with(&mut q, std::slice::from_ref(&live)).unwrap();
        assert_eq!(r.takes, 1);
        assert!(r.saved.is_ok());
        assert_eq!(std::fs::read(&live_path).unwrap(), before, "fichier intact");
        let files: Vec<_> = q.tracks[0].clips.iter().map(|c| c.file.as_str()).collect();
        assert_eq!(files, ["Voix_001.wav"]);
        // Le journal ne garde que la prise encore en cours.
        let text = std::fs::read_to_string(journal_path(&dir)).unwrap();
        let kept: Journal = serde_json::from_str(&text).unwrap();
        assert!(kept.takes.is_empty());
        assert_eq!(kept.extra.len(), 1);
        assert_eq!(kept.extra[0].takes[0].file, "Voix_002.wav");
        // Reportée dans un nouveau journal : seulement les prises qui ne sont pas en cours.
        assert!(kept.pending_groups(std::slice::from_ref(&live)).is_empty());
        assert_eq!(kept.pending_groups(&[]).len(), 1);
        std::fs::remove_dir_all(dir).ok();
    }

    /// Sauvegarde de la session impossible après une récupération : positions notées et
    /// journal gardés, une nouvelle récupération replace les fichiers au même endroit.
    #[test]
    fn recover_keeps_positions_until_saved() {
        let dir = std::env::temp_dir().join(format!("tp-recover-unsaved-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut p = Project::new("U".into(), dir.clone(), 48000);
        p.ensure_dirs().unwrap();
        let t = p.add_track(Some("Voix".into()), Some(0));
        p.save().unwrap();
        write_wav(&p.audio_dir().join("Voix_001.wav"), 4800);
        write_wav(&p.audio_dir().join("Voix_001_p2.wav"), 2000);
        let side = p
            .audio_dir()
            .join(crate::recorder::start_sidecar("Voix_001_p2.wav"));
        std::fs::write(&side, "5800").unwrap();
        let j = Journal {
            start: 0,
            play_offset: 0,
            latency: 0,
            sample_rate: 48000,
            extra: Vec::new(),
            takes: vec![JournalTake {
                track_id: t,
                file: "Voix_001.wav".into(),
            }],
        };
        write_journal(&dir, &j).unwrap();
        // Sauvegarde impossible (session protégée : toute écriture est refusée).
        let mut q = Project::load(&p.file_path()).unwrap();
        q.read_only = Some("test : sauvegarde refusée".into());
        let r = recover(&mut q).unwrap();
        assert_eq!(r.takes, 2);
        assert!(r.saved.is_err());
        assert!(side.exists(), "position gardée");
        assert!(journal_path(&dir).exists(), "journal gardé");
        // Plus tard (crash entre-temps : session d'origine), la récupération recommence.
        let mut q = Project::load(&p.file_path()).unwrap();
        let r = recover(&mut q).unwrap();
        assert!(r.saved.is_ok());
        let c: Vec<_> = q.tracks[0].clips.iter().map(|c| c.start).collect();
        assert_eq!(c, [0, 5800]);
        assert!(!side.exists());
        assert!(!journal_path(&dir).exists());
        std::fs::remove_dir_all(dir).ok();
    }

    /// Liste des sessions à récupérer : ajout en tête sans doublon, retrait.
    #[test]
    fn pending_list_add_and_remove() {
        let a = Path::new("C:/Sessions/A");
        let b = Path::new("D:/B");
        let l1 = pending_with("", a, true).unwrap();
        assert_eq!(pending_dirs(&l1), [a]);
        assert!(pending_with(&l1, a, true).is_none(), "déjà présente");
        let l2 = pending_with(&l1, b, true).unwrap();
        assert_eq!(pending_dirs(&l2), [b, a]);
        let l3 = pending_with(&l2, a, false).unwrap();
        assert_eq!(pending_dirs(&l3), [b]);
        assert!(pending_with(&l3, a, false).is_none());
    }

    /// Journal écrit en retard par un thread resté bloqué : jamais par-dessus un journal
    /// décidé après lui, ni après la suppression du journal.
    #[test]
    fn older_journal_never_replaces_newer() {
        let dir = std::env::temp_dir().join(format!("tp-journal-rank-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let j = |start: u64| Journal {
            start,
            play_offset: 0,
            latency: 0,
            sample_rate: 48000,
            extra: Vec::new(),
            takes: Vec::new(),
        };
        let (old, new) = (next_journal_rank(), next_journal_rank());
        write_journal_ranked(&dir, &j(2), new).unwrap();
        write_journal_ranked(&dir, &j(1), old).unwrap();
        let read = || {
            std::fs::read_to_string(journal_path(&dir))
                .ok()
                .map(|t| serde_json::from_str::<Journal>(&t).unwrap().start)
        };
        assert_eq!(read(), Some(2), "le plus récent reste");
        remove_journal(&dir);
        write_journal_ranked(&dir, &j(1), old).unwrap();
        assert_eq!(read(), None, "journal supprimé jamais ressuscité");
        write_journal(&dir, &j(3)).unwrap();
        assert_eq!(read(), Some(3));
        std::fs::remove_dir_all(dir).ok();
    }
}
