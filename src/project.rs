//! Modèle de session : pistes, prises (clips) et sauvegarde JSON.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
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
}

impl Clip {
    pub fn end(&self) -> u64 {
        self.start + self.len
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Track {
    pub id: u64,
    pub name: String,
    /// Canal d'entrée de la carte son (0-based). `None` = pas d'entrée.
    pub input: Option<u16>,
    pub armed: bool,
    /// Les prises récentes sont à la fin et passent au-dessus des anciennes.
    pub clips: Vec<Clip>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Project {
    pub name: String,
    pub sample_rate: u32,
    #[serde(default)]
    pub rec_format: RecFormat,
    pub tracks: Vec<Track>,
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
            clips: Vec::new(),
        });
        self.dirty = true;
        id
    }

    pub fn track_mut(&mut self, id: u64) -> Option<&mut Track> {
        self.tracks.iter_mut().find(|t| t.id == id)
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
        std::fs::write(&tmp, json)?;
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
        let old_audio = self.audio_dir();
        let old_dir = self.dir.clone();
        self.dir = new_dir;
        self.name = new_name;
        if let Err(e) = self.ensure_dirs() {
            self.dir = old_dir;
            return Err(e);
        }
        let new_audio = self.audio_dir();
        if old_audio != new_audio {
            for t in &self.tracks {
                for c in &t.clips {
                    let src = old_audio.join(&c.file);
                    let dst = new_audio.join(&c.file);
                    if src.exists() && !dst.exists() {
                        std::fs::copy(&src, &dst)
                            .with_context(|| format!("Copie de {}", src.display()))?;
                    }
                }
            }
        }
        self.save()
    }

    /// Nom de fichier unique pour une nouvelle prise.
    pub fn new_take_file(&self, track_name: &str) -> String {
        let base = sanitize(track_name);
        let dir = self.audio_dir();
        for n in 1.. {
            let f = format!("{base}_{n:03}.wav");
            if !dir.join(&f).exists() {
                return f;
            }
        }
        unreachable!()
    }
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
        });
        p.save().unwrap();
        let q = Project::load(&p.file_path()).unwrap();
        assert_eq!(q.tracks.len(), 1);
        assert_eq!(q.end(), 110);
        assert_eq!(q.dir, dir);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn sanitize_names() {
        assert_eq!(sanitize("Voix/Lead:1"), "Voix_Lead_1");
        assert_eq!(sanitize("  "), "Piste");
    }
}
