//! Modèle de session : pistes, prises (clips) et sauvegarde JSON.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub const PROJECT_EXT: &str = "tracksplus";
pub const AUDIO_DIR: &str = "Audio";

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum RecFormat {
    #[default]
    Int24,
    Float32,
}

impl RecFormat {
    pub fn label(self) -> &'static str {
        match self {
            RecFormat::Int24 => "24 bits",
            RecFormat::Float32 => "32 bits flottant",
        }
    }
}

/// Une prise audio posée sur la timeline d'une piste.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Clip {
    pub id: u64,
    /// Nom du fichier dans le dossier `Audio/` de la session.
    pub file: String,
    /// Position sur la timeline (en échantillons).
    pub start: u64,
    /// Décalage dans le fichier (en échantillons).
    #[serde(default)]
    pub offset: u64,
    /// Longueur (en échantillons).
    pub len: u64,
    /// Longueur totale du fichier source (0 = inconnue : on ne peut pas rallonger).
    #[serde(default)]
    pub src_len: u64,
    /// Fondu d'entrée (en échantillons depuis le début de la prise).
    #[serde(default)]
    pub fade_in: u64,
    /// Fondu de sortie (en échantillons avant la fin de la prise).
    #[serde(default)]
    pub fade_out: u64,
}

/// Longueur minimale d'une prise après rognage ou coupe (en échantillons).
pub const MIN_CLIP: u64 = 64;

impl Clip {
    pub fn end(&self) -> u64 {
        self.start + self.len
    }

    /// Fin du fichier source, en échantillons depuis le début du fichier.
    pub fn src_end(&self) -> u64 {
        if self.src_len > 0 {
            self.src_len
        } else {
            self.offset + self.len
        }
    }

    /// Garde les fondus dans la longueur de la prise.
    pub fn clamp_fades(&mut self) {
        self.fade_in = self.fade_in.min(self.len);
        self.fade_out = self.fade_out.min(self.len - self.fade_in);
    }

    /// Gain du fondu à la position `t` (timeline) : courbe en S, 1 hors fondus.
    pub fn fade_gain(&self, t: u64) -> f32 {
        let mut g = 1.0f32;
        let rel = t.saturating_sub(self.start);
        if rel < self.fade_in {
            g *= s_curve(rel as f32 / self.fade_in as f32);
        }
        let to_end = self.end().saturating_sub(t);
        if to_end < self.fade_out {
            g *= s_curve(to_end as f32 / self.fade_out as f32);
        }
        g
    }
}

/// Courbe de fondu douce (sinus²), sans clic aux extrémités.
pub fn s_curve(x: f32) -> f32 {
    let s = (x.clamp(0.0, 1.0) * std::f32::consts::FRAC_PI_2).sin();
    s * s
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Track {
    pub id: u64,
    pub name: String,
    /// Canal d'entrée de la carte son (0-based). `None` = pas d'entrée.
    pub input: Option<u16>,
    pub armed: bool,
    #[serde(default)]
    pub mute: bool,
    #[serde(default)]
    pub solo: bool,
    /// Les prises récentes sont à la fin et passent au-dessus des anciennes.
    pub clips: Vec<Clip>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Marker {
    pub id: u64,
    pub pos: u64,
    pub name: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Project {
    pub name: String,
    pub sample_rate: u32,
    #[serde(default)]
    pub rec_format: RecFormat,
    pub tracks: Vec<Track>,
    #[serde(default)]
    pub markers: Vec<Marker>,
    pub next_id: u64,
    /// Dossier de la session (non sérialisé).
    #[serde(skip)]
    pub dir: PathBuf,
    /// Modifié depuis la dernière sauvegarde.
    #[serde(skip)]
    pub dirty: bool,
}

impl Project {
    pub fn new(name: String, dir: PathBuf, sample_rate: u32) -> Self {
        Self {
            name,
            sample_rate,
            rec_format: RecFormat::Int24,
            tracks: Vec::new(),
            markers: Vec::new(),
            next_id: 1,
            dir,
            dirty: false,
        }
    }

    /// Nouvelle session horodatée dans le dossier par défaut.
    pub fn untitled(sample_rate: u32) -> Self {
        let name = chrono::Local::now()
            .format("Session %Y-%m-%d %Hh%M")
            .to_string();
        let dir = default_sessions_root().join(&name);
        Self::new(name, dir, sample_rate)
    }

    pub fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn add_track(&mut self, name: Option<String>, input: Option<u16>) -> u64 {
        let id = self.alloc_id();
        let name = name.unwrap_or_else(|| format!("Piste {}", self.tracks.len() + 1));
        self.tracks.push(Track {
            id,
            name,
            input,
            armed: input.is_some(),
            mute: false,
            solo: false,
            clips: Vec::new(),
        });
        self.dirty = true;
        id
    }

    pub fn track_mut(&mut self, id: u64) -> Option<&mut Track> {
        self.tracks.iter_mut().find(|t| t.id == id)
    }

    /// Pistes audibles en lecture (mute / solo).
    pub fn audible(&self) -> Vec<bool> {
        let any_solo = self.tracks.iter().any(|t| t.solo);
        self.tracks
            .iter()
            .map(|t| if any_solo { t.solo } else { !t.mute })
            .collect()
    }

    pub fn add_marker(&mut self, pos: u64) -> u64 {
        let id = self.alloc_id();
        let n = self.markers.len() + 1;
        self.markers.push(Marker {
            id,
            pos,
            name: format!("Repère {n}"),
        });
        self.markers.sort_by_key(|m| m.pos);
        self.dirty = true;
        id
    }

    /// Coupe une prise en deux à `pos`. Retourne l'identifiant de la nouvelle moitié droite.
    pub fn split_clip(&mut self, track_id: u64, clip_id: u64, pos: u64) -> Option<u64> {
        let new_id = self.next_id;
        let t = self.tracks.iter_mut().find(|t| t.id == track_id)?;
        let i = t.clips.iter().position(|c| c.id == clip_id)?;
        let c = t.clips[i].clone();
        if pos < c.start + MIN_CLIP || pos + MIN_CLIP > c.end() {
            return None;
        }
        let left = pos - c.start;
        t.clips[i].len = left;
        t.clips[i].fade_out = 0;
        t.clips[i].clamp_fades();
        let mut right = Clip {
            id: new_id,
            file: c.file.clone(),
            start: pos,
            offset: c.offset + left,
            len: c.len - left,
            src_len: c.src_len,
            fade_in: 0,
            fade_out: c.fade_out,
        };
        right.clamp_fades();
        // Juste après la moitié gauche : l'ordre de superposition est conservé.
        t.clips.insert(i + 1, right);
        self.next_id += 1;
        self.dirty = true;
        Some(new_id)
    }

    /// Coupe à `pos` toutes les prises concernées. `clips` limite aux prises données,
    /// `tracks` aux pistes données. Retourne les couples (ancienne, nouvelle) prise.
    pub fn split_at(
        &mut self,
        pos: u64,
        tracks: Option<&HashSet<u64>>,
        clips: Option<&HashSet<u64>>,
    ) -> Vec<(u64, u64)> {
        let targets: Vec<(u64, u64)> = self
            .tracks
            .iter()
            .filter(|t| tracks.is_none_or(|s| s.contains(&t.id)))
            .flat_map(|t| {
                t.clips
                    .iter()
                    .filter(|c| c.start < pos && pos < c.end())
                    .filter(|c| clips.is_none_or(|s| s.contains(&c.id)))
                    .map(move |c| (t.id, c.id))
            })
            .collect();
        targets
            .into_iter()
            .filter_map(|(t, c)| self.split_clip(t, c, pos).map(|n| (c, n)))
            .collect()
    }

    /// Efface le contenu entre `a` et `b` (sans décaler la suite).
    pub fn delete_range(
        &mut self,
        a: u64,
        b: u64,
        tracks: Option<&HashSet<u64>>,
    ) -> Vec<(u64, u64)> {
        let mut pairs = self.split_at(a, tracks, None);
        pairs.extend(self.split_at(b, tracks, None));
        for t in &mut self.tracks {
            if tracks.is_none_or(|s| s.contains(&t.id)) {
                t.clips.retain(|c| !(c.start >= a && c.end() <= b));
            }
        }
        self.dirty = true;
        pairs
    }

    pub fn find_clip(&self, clip_id: u64) -> Option<(usize, &Clip)> {
        self.tracks
            .iter()
            .enumerate()
            .find_map(|(i, t)| t.clips.iter().find(|c| c.id == clip_id).map(|c| (i, c)))
    }

    pub fn has_audio(&self) -> bool {
        self.tracks.iter().any(|t| !t.clips.is_empty())
    }

    /// Fin de la dernière prise (en échantillons).
    pub fn end(&self) -> u64 {
        self.tracks
            .iter()
            .flat_map(|t| t.clips.iter().map(Clip::end))
            .max()
            .unwrap_or(0)
    }

    pub fn audio_dir(&self) -> PathBuf {
        self.dir.join(AUDIO_DIR)
    }

    pub fn file_path(&self) -> PathBuf {
        self.dir.join(format!("{}.{}", self.name, PROJECT_EXT))
    }

    pub fn clip_path(&self, clip: &Clip) -> PathBuf {
        self.audio_dir().join(&clip.file)
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(self.audio_dir())
            .with_context(|| format!("Impossible de créer {}", self.audio_dir().display()))
    }

    pub fn save(&mut self) -> Result<()> {
        self.ensure_dirs()?;
        let json = serde_json::to_string_pretty(self)?;
        let path = self.file_path();
        let tmp = path.with_extension("tmp");
        write_synced(&tmp, json.as_bytes())?;
        std::fs::rename(&tmp, &path)?;
        self.dirty = false;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self> {
        let json = std::fs::read_to_string(path)
            .with_context(|| format!("Lecture de {}", path.display()))?;
        let mut p: Project = serde_json::from_str(&json).context("Fichier de session invalide")?;
        p.dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        p.name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or(p.name);
        p.dirty = false;
        Ok(p)
    }

    /// "Enregistrer sous" : copie les fichiers audio dans le nouveau dossier.
    pub fn save_as(&mut self, new_dir: PathBuf, new_name: String) -> Result<()> {
        // On prépare tout sur une copie : la session courante n'est modifiée qu'une fois
        // la copie des fichiers et la sauvegarde réussies.
        let mut next = self.clone();
        next.dir = new_dir;
        next.name = new_name;
        next.ensure_dirs()?;
        let old_audio = self.audio_dir();
        let new_audio = next.audio_dir();
        if old_audio != new_audio {
            let mut files: Vec<&str> = self
                .tracks
                .iter()
                .flat_map(|t| t.clips.iter().map(|c| c.file.as_str()))
                .collect();
            files.sort();
            files.dedup();
            for f in files {
                let src = old_audio.join(f);
                let dst = new_audio.join(f);
                if src.exists() && !dst.exists() {
                    let tmp = dst.with_extension("copie");
                    std::fs::copy(&src, &tmp)
                        .with_context(|| format!("Copie de {}", src.display()))?;
                    std::fs::rename(&tmp, &dst)?;
                }
            }
        }
        next.save()?;
        *self = next;
        Ok(())
    }

    /// Nom de fichier unique pour une nouvelle prise.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new_take_file(&self, track_name: &str) -> String {
        self.new_take_file_avoiding(track_name, &HashSet::new())
    }

    /// Nom unique, sans collision avec les fichiers existants (y compris les suites
    /// `_p2` d'une prise longue) ni avec `taken`, sans tenir compte de la casse
    /// (Windows et macOS ne distinguent pas « Voix » et « voix »).
    pub fn new_take_file_avoiding(&self, track_name: &str, taken: &HashSet<String>) -> String {
        let base = sanitize(track_name);
        let existing: HashSet<String> = std::fs::read_dir(self.audio_dir())
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().to_lowercase())
                    .collect()
            })
            .unwrap_or_default();
        for n in 1.. {
            let f = format!("{base}_{n:03}.wav");
            let low = f.to_lowercase();
            let p2 = format!("{base}_{n:03}_p2.wav").to_lowercase();
            if !existing.contains(&low) && !existing.contains(&p2) && !taken.contains(&low) {
                return f;
            }
        }
        unreachable!()
    }
}

/// Fusion pour Annuler / Rétablir : prend les prises de `snap`, garde la configuration
/// des pistes de `current` (armement, entrées, mute/solo, noms), rétablit les pistes
/// supprimées depuis et conserve celles créées depuis.
pub fn merge_clips(mut current: Vec<Track>, snap: Vec<Track>) -> Vec<Track> {
    let mut merged = Vec::with_capacity(current.len().max(snap.len()));
    for st in snap {
        match current.iter().position(|t| t.id == st.id) {
            Some(i) => {
                let mut t = current.remove(i);
                t.clips = st.clips;
                merged.push(t);
            }
            None => merged.push(st),
        }
    }
    merged.extend(current);
    merged
}

/// Écrit un fichier et force son contenu sur le disque (résiste à une coupure de courant).
pub fn write_synced(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(path)?;
    f.write_all(data)?;
    f.sync_all()
}

pub fn sanitize(s: &str) -> String {
    let out: String = s
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || " -_".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    let out = out.trim().to_string();
    if out.is_empty() { "Piste".into() } else { out }
}

pub fn default_sessions_root() -> PathBuf {
    dirs::audio_dir()
        .or_else(dirs::document_dir)
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("TracksPlus")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let dir = std::env::temp_dir().join(format!("tp-test-{}", std::process::id()));
        let mut p = Project::new("Test".into(), dir.clone(), 48000);
        let id = p.add_track(None, Some(0));
        p.track_mut(id).unwrap().clips.push(Clip {
            id: 99,
            file: "a.wav".into(),
            start: 10,
            offset: 0,
            len: 100,
            src_len: 100,
            fade_in: 0,
            fade_out: 0,
        });
        p.save().unwrap();
        let q = Project::load(&p.file_path()).unwrap();
        assert_eq!(q.tracks.len(), 1);
        assert_eq!(q.end(), 110);
        assert_eq!(q.dir, dir);
        std::fs::remove_dir_all(dir).ok();
    }

    fn clip_project() -> (Project, u64) {
        let mut p = Project::new("E".into(), std::env::temp_dir(), 48000);
        let t = p.add_track(None, Some(0));
        let id = p.alloc_id();
        p.track_mut(t).unwrap().clips.push(Clip {
            id,
            file: "a.wav".into(),
            start: 1000,
            offset: 0,
            len: 10_000,
            src_len: 10_000,
            fade_in: 500,
            fade_out: 800,
        });
        (p, t)
    }

    #[test]
    fn split_keeps_audio_continuous() {
        let (mut p, t) = clip_project();
        let pairs = p.split_at(4000, None, None);
        assert_eq!(pairs.len(), 1);
        let c = &p.track_mut(t).unwrap().clips;
        assert_eq!((c[0].start, c[0].len), (1000, 3000));
        assert_eq!((c[1].start, c[1].offset, c[1].len), (4000, 3000, 7000));
        assert_eq!(c[1].end(), 11_000);
        // Les fondus restent aux extrémités d'origine.
        assert_eq!((c[0].fade_in, c[0].fade_out), (500, 0));
        assert_eq!((c[1].fade_in, c[1].fade_out), (0, 800));
        // Coupe hors prise ou trop près d'un bord : rien.
        assert!(p.split_at(500, None, None).is_empty());
        assert!(p.split_at(1010, None, None).is_empty());
    }

    #[test]
    fn fade_gain_shape() {
        let c = Clip {
            id: 1,
            file: String::new(),
            start: 100,
            offset: 0,
            len: 1000,
            src_len: 1000,
            fade_in: 100,
            fade_out: 200,
        };
        assert_eq!(c.fade_gain(100), 0.0);
        assert!((c.fade_gain(150) - 0.5).abs() < 1e-3);
        assert_eq!(c.fade_gain(500), 1.0);
        assert!(c.fade_gain(1099) < 0.01);
    }

    #[test]
    fn delete_range_removes_middle() {
        let (mut p, t) = clip_project();
        p.delete_range(3000, 5000, None);
        let c = &p.track_mut(t).unwrap().clips;
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].end(), 3000);
        assert_eq!((c[1].start, c[1].offset), (5000, 4000));
    }

    #[test]
    fn undo_merge_keeps_track_config() {
        let (mut p, t) = clip_project();
        let snap = p.tracks.clone();
        // Après l'instantané : coupe, réglages de piste, nouvelle piste.
        p.split_at(4000, None, None);
        p.track_mut(t).unwrap().armed = false;
        p.track_mut(t).unwrap().input = Some(5);
        let t2 = p.add_track(Some("Nouvelle".into()), Some(1));
        let merged = merge_clips(p.tracks.clone(), snap);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].clips.len(), 1, "montage annulé");
        assert_eq!(merged[0].input, Some(5), "configuration conservée");
        assert!(!merged[0].armed);
        assert_eq!(merged[1].id, t2, "piste créée depuis conservée");
    }

    #[test]
    fn take_names_never_collide() {
        let dir = std::env::temp_dir().join(format!("tp-names-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p = Project::new("N".into(), dir.clone(), 48000);
        p.ensure_dirs().unwrap();
        std::fs::write(p.audio_dir().join("Voix_001.wav"), b"x").unwrap();
        std::fs::write(p.audio_dir().join("Voix_002_p2.wav"), b"x").unwrap();
        // Existant (autre casse) et suite _p2 : on saute à 003.
        assert_eq!(p.new_take_file("voix"), "voix_003.wav");
        let taken: HashSet<String> = ["voix_003.wav".to_string()].into();
        assert_eq!(p.new_take_file_avoiding("VOIX", &taken), "VOIX_004.wav");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn sanitize_names() {
        assert_eq!(sanitize("Voix/Lead:1"), "Voix_Lead_1");
        assert_eq!(sanitize("  "), "Piste");
    }
}
