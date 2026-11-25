//! Modèle de session : pistes, prises (clips) et sauvegarde JSON.

use anyhow::{Context, Result, bail};
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
    #[serde(default)]
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

/// Position maximale acceptée dans une session (≈ 2,9 ans à 192 kHz) : un fichier abîmé
/// ou fabriqué ne peut pas provoquer de débordement dans les calculs de positions.
pub const MAX_POS: u64 = 1 << 44;

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

/// Sorte de piste.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TrackKind {
    /// Piste d'enregistrement : entrée de la carte, armement, monitoring.
    #[default]
    Record,
    /// Piste audio : fichiers importés (musique, sons…), lecture seule, sans entrée ; mono
    /// ou stéréo selon les fichiers.
    Audio,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Track {
    pub id: u64,
    #[serde(default)]
    pub kind: TrackKind,
    #[serde(default)]
    pub name: String,
    /// Canal d'entrée de la carte son (0-based). `None` = pas d'entrée.
    /// Piste stéréo : premier canal de la paire (`input`, `input + 1`).
    #[serde(default)]
    pub input: Option<u16>,
    /// Piste stéréo : enregistre une paire d'entrées dans un seul WAV stéréo.
    #[serde(default)]
    pub stereo: bool,
    #[serde(default)]
    pub armed: bool,
    #[serde(default)]
    pub mute: bool,
    #[serde(default)]
    pub solo: bool,
    /// Monitoring direct : l'entrée de la piste est envoyée en temps réel sur la sortie
    /// de monitoring.
    #[serde(default)]
    pub monitor: bool,
    /// Couleur choisie (indice dans la palette) ; `None` = couleur selon la position.
    #[serde(default)]
    pub color: Option<u8>,
    /// Les prises récentes sont à la fin et passent au-dessus des anciennes.
    #[serde(default)]
    pub clips: Vec<Clip>,
}

impl Track {
    pub fn is_audio(&self) -> bool {
        self.kind == TrackKind::Audio
    }

    /// Nombre de canaux de la piste (1 = mono, 2 = stéréo).
    pub fn channels(&self) -> usize {
        if self.stereo { 2 } else { 1 }
    }

    /// Canaux de la carte utilisés par la piste.
    pub fn inputs(&self) -> impl Iterator<Item = u16> + '_ {
        self.input
            .into_iter()
            .flat_map(move |i| (0..self.channels() as u16).map(move |k| i.saturating_add(k)))
    }

    /// L'entrée (ou la paire) existe sur une carte de `in_ch` canaux.
    pub fn input_fits(&self, in_ch: u16) -> bool {
        self.input
            .is_some_and(|i| (i as usize) + self.channels() <= in_ch as usize)
    }

    /// Libellé court de l'entrée : « IN 3 », « IN 3-4 » ou « IN — ».
    pub fn input_label(&self) -> String {
        match self.input {
            Some(i) if self.stereo => format!("IN {}-{}", i + 1, i + 2),
            Some(i) => format!("IN {}", i + 1),
            None => "IN —".into(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Marker {
    pub id: u64,
    pub pos: u64,
    #[serde(default)]
    pub name: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Project {
    /// Nom (remplacé au chargement par celui du fichier).
    #[serde(default)]
    pub name: String,
    #[serde(default = "default_sample_rate")]
    pub sample_rate: u32,
    #[serde(default)]
    pub rec_format: RecFormat,
    #[serde(default)]
    pub tracks: Vec<Track>,
    #[serde(default)]
    pub markers: Vec<Marker>,
    /// Recalculé au chargement s'il est incohérent.
    #[serde(default)]
    pub next_id: u64,
    /// Dossier de la session (non sérialisé).
    #[serde(skip)]
    pub dir: PathBuf,
    /// Modifié depuis la dernière sauvegarde.
    #[serde(skip)]
    pub dirty: bool,
}

/// Fréquence par défaut (fichier sans fréquence ou fréquence nulle).
pub const DEFAULT_SAMPLE_RATE: u32 = 48000;

fn default_sample_rate() -> u32 {
    DEFAULT_SAMPLE_RATE
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

    /// Ajoute une piste mono.
    pub fn add_track(&mut self, name: Option<String>, input: Option<u16>) -> u64 {
        self.add_track_ch(name, input, false)
    }

    /// Ajoute une piste mono ou stéréo (`input` = premier canal de la paire).
    pub fn add_track_ch(&mut self, name: Option<String>, input: Option<u16>, stereo: bool) -> u64 {
        let id = self.alloc_id();
        let name = name.unwrap_or_else(|| format!("Piste {}", self.tracks.len() + 1));
        self.tracks.push(Track {
            id,
            kind: TrackKind::Record,
            name,
            input,
            stereo,
            armed: input.is_some(),
            mute: false,
            solo: false,
            monitor: false,
            color: None,
            clips: Vec::new(),
        });
        self.dirty = true;
        id
    }

    /// Ajoute une piste audio (fichiers importés), sans entrée.
    pub fn add_audio_track(&mut self, name: String, stereo: bool) -> u64 {
        let id = self.add_track_ch(Some(name), None, stereo);
        if let Some(t) = self.track_mut(id) {
            t.kind = TrackKind::Audio;
            t.armed = false;
        }
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
        // Défense en profondeur : `validate` assainit déjà les noms au chargement.
        self.audio_dir().join(safe_file_name(&clip.file))
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(self.audio_dir())
            .with_context(|| format!("Impossible de créer {}", self.audio_dir().display()))
    }

    pub fn save(&mut self) -> Result<()> {
        let seq = next_save_seq();
        self.write_file(seq)?;
        self.dirty = false;
        Ok(())
    }

    /// Sauvegarde dans un thread (sauvegarde automatique : un disque lent ne fige pas
    /// l'interface). L'état est figé maintenant ; si une sauvegarde plus récente a déjà été
    /// écrite entre-temps, celle-ci est abandonnée (jamais d'ancien état par-dessus).
    pub fn save_in_background(&mut self) -> std::sync::mpsc::Receiver<Result<()>> {
        let seq = next_save_seq();
        let snapshot = self.clone();
        self.dirty = false;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(snapshot.write_file(seq));
        });
        rx
    }

    /// Écriture atomique du fichier de session (fichier temporaire synchronisé puis
    /// renommage), une à la fois, et seulement si rien de plus récent n'a été écrit.
    fn write_file(&self, seq: u64) -> Result<()> {
        static WRITTEN: std::sync::Mutex<Vec<(PathBuf, u64)>> = std::sync::Mutex::new(Vec::new());
        let path = self.file_path();
        let mut written = WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
        if written.iter().any(|(p, s)| *p == path && *s > seq) {
            return Ok(());
        }
        self.ensure_dirs()?;
        let json = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("tmp");
        write_synced(&tmp, json.as_bytes())?;
        std::fs::rename(&tmp, &path)?;
        written.retain(|(p, _)| *p != path);
        written.push((path, seq));
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
        p.validate();
        p.dirty = false;
        Ok(p)
    }

    /// Corrige un fichier incohérent (ancien, partiel ou modifié à la main) : fréquence
    /// nulle, identifiant suivant déjà utilisé, fondus plus longs que la prise.
    pub fn validate(&mut self) {
        if !(8_000..=768_000).contains(&self.sample_rate) {
            self.sample_rate = DEFAULT_SAMPLE_RATE;
        }
        for m in &mut self.markers {
            m.pos = m.pos.min(MAX_POS);
        }
        self.renumber_if_broken();
        let max_id = self
            .tracks
            .iter()
            .flat_map(|t| std::iter::once(t.id).chain(t.clips.iter().map(|c| c.id)))
            .chain(self.markers.iter().map(|m| m.id))
            .max()
            .unwrap_or(0);
        self.next_id = self.next_id.max(max_id.saturating_add(1));
        for (i, t) in self.tracks.iter_mut().enumerate() {
            if t.name.trim().is_empty() {
                t.name = format!("Piste {}", i + 1);
            }
            // Prise sans nom de fichier utilisable : rien à lire, et elle ferait échouer
            // « Enregistrer sous » (son chemin serait le dossier Audio lui-même).
            t.clips.retain(|c| !safe_file_name(&c.file).is_empty());
            for c in &mut t.clips {
                c.start = c.start.min(MAX_POS);
                c.offset = c.offset.min(MAX_POS);
                c.len = c.len.min(MAX_POS);
                c.src_len = c.src_len.min(MAX_POS);
                c.clamp_fades();
                // Un fichier de session partagé ne doit jamais faire lire ou écrire hors du
                // dossier Audio/ (« ../../ », chemin absolu, partage réseau \\hôte\…).
                let safe = safe_file_name(&c.file);
                if safe != c.file {
                    eprintln!("Nom de fichier de prise refusé : {:?} → {:?}", c.file, safe);
                    c.file = safe;
                }
            }
        }
    }

    /// Identifiants en double ou démesurés (fichier abîmé ou fabriqué) : tout est
    /// renuméroté, sinon les prises se confondraient et le compteur déborderait.
    fn renumber_if_broken(&mut self) {
        let ids: Vec<u64> = self
            .tracks
            .iter()
            .flat_map(|t| std::iter::once(t.id).chain(t.clips.iter().map(|c| c.id)))
            .chain(self.markers.iter().map(|m| m.id))
            .collect();
        let unique: HashSet<u64> = ids.iter().copied().collect();
        if unique.len() == ids.len() && ids.iter().all(|&i| i < MAX_POS) && self.next_id < MAX_POS {
            return;
        }
        let mut next = 1;
        let mut alloc = || {
            next += 1;
            next - 1
        };
        for t in &mut self.tracks {
            t.id = alloc();
            for c in &mut t.clips {
                c.id = alloc();
            }
        }
        for m in &mut self.markers {
            m.id = alloc();
        }
        self.next_id = alloc();
    }

    /// "Enregistrer sous" : copie les fichiers audio dans le nouveau dossier.
    /// Utilisable sur une copie de la session depuis un autre thread.
    /// Renvoie les fichiers audio introuvables (non copiés) : la session est enregistrée,
    /// mais ces prises seront muettes.
    pub fn save_as(&mut self, new_dir: PathBuf, new_name: String) -> Result<Vec<String>> {
        let mut missing = Vec::new();
        // On prépare tout sur une copie : la session courante n'est modifiée qu'une fois
        // la copie des fichiers et la sauvegarde réussies.
        let mut next = self.clone();
        next.dir = new_dir;
        next.name = new_name;
        let target = next.file_path();
        if target.exists() && target != self.file_path() {
            bail!(
                "Une session « {} » existe déjà dans ce dossier ({})",
                next.name,
                next.dir.display()
            );
        }
        next.ensure_dirs()?;
        let old_audio = self.audio_dir();
        let new_audio = next.audio_dir();
        if old_audio != new_audio {
            let mut files: Vec<String> = self
                .tracks
                .iter()
                .flat_map(|t| t.clips.iter().map(|c| c.file.clone()))
                .collect();
            files.sort();
            files.dedup();
            // Noms déjà pris dans le dossier cible (sans tenir compte de la casse).
            let mut taken: HashSet<String> = std::fs::read_dir(&new_audio)
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .map(|e| e.file_name().to_string_lossy().to_lowercase())
                        .collect()
                })
                .unwrap_or_default();
            let mut renamed: std::collections::HashMap<String, String> = Default::default();
            for f in files {
                let src = old_audio.join(&f);
                if !src.exists() {
                    missing.push(f.clone());
                    continue;
                }
                // Un fichier du même nom existe déjà dans la cible : jamais réutilisé tel
                // quel (ce n'est peut-être pas le même son), copie sous un nouveau nom.
                let name = if taken.contains(&f.to_lowercase()) {
                    let n = unique_name(&f, &taken);
                    renamed.insert(f.clone(), n.clone());
                    n
                } else {
                    f.clone()
                };
                taken.insert(name.to_lowercase());
                copy_synced(&src, &new_audio.join(&name))?;
            }
            if !renamed.is_empty() {
                for c in next.tracks.iter_mut().flat_map(|t| t.clips.iter_mut()) {
                    if let Some(n) = renamed.get(&c.file) {
                        c.file = n.clone();
                    }
                }
            }
        }
        next.save()?;
        *self = next;
        Ok(missing)
    }

    /// Nom de fichier unique pour une nouvelle prise.
    #[cfg(test)]
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
/// supprimées depuis. Les pistes créées depuis reviennent à leur état d'alors : sans
/// prise (une piste d'enregistrement reste, réglages compris) ; une piste audio (import,
/// glisser-déposer) n'existait pas, elle disparaît — annuler un import l'annule vraiment.
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
    for mut t in current {
        if t.is_audio() {
            continue;
        }
        t.clips.clear();
        merged.push(t);
    }
    merged
}

/// Écrit un fichier et force son contenu sur le disque (résiste à une coupure de courant).
pub fn write_synced(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(path)?;
    f.write_all(data)?;
    f.sync_all()
}

/// Écriture atomique : fichier temporaire voisin forcé sur le disque, puis renommé. Une
/// coupure en pleine écriture laisse l'ancien fichier intact, jamais un fichier tronqué.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(d) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(d).with_context(|| format!("Création de {}", d.display()))?;
    }
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    let tmp = path.with_file_name(name);
    write_synced(&tmp, bytes).with_context(|| format!("Écriture de {}", tmp.display()))?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("Écriture de {}", path.display()));
    }
    Ok(())
}

/// Copie `src` vers `dst` via un fichier temporaire forcé sur le disque, puis renommé.
pub fn copy_synced(src: &Path, dst: &Path) -> Result<()> {
    let mut name = dst.file_name().unwrap_or_default().to_os_string();
    name.push(".copie");
    let tmp = dst.with_file_name(name);
    let res = (|| -> Result<()> {
        std::fs::copy(src, &tmp).with_context(|| format!("Copie de {}", src.display()))?;
        // Accès en écriture : exigé par Windows pour forcer l'écriture sur le disque.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&tmp)?
            .sync_all()?;
        std::fs::rename(&tmp, dst)?;
        Ok(())
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

/// Numéro d'ordre des sauvegardes (voir `Project::write_file`).
fn next_save_seq() -> u64 {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Ramène un nom de fichier de prise à un simple nom, sans dossier ni lecteur :
/// « ../../x.wav », « C:\\x.wav » ou « \\\\hôte\\x.wav » deviennent « x.wav ».
/// Renvoie une chaîne vide si rien d'utilisable ne reste (prise muette).
pub fn safe_file_name(file: &str) -> String {
    let last = file.rsplit(['/', '\\', ':']).next().unwrap_or("");
    if last.trim().is_empty() || last == "." || last == ".." || last.contains('\0') {
        return String::new();
    }
    last.to_string()
}

/// Premier nom libre « nom (2).wav », « nom (3).wav »… (`taken` en minuscules).
fn unique_name(file: &str, taken: &HashSet<String>) -> String {
    let p = Path::new(file);
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = p
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    (2..)
        .map(|n| format!("{stem} ({n}){ext}"))
        .find(|n| !taken.contains(&n.to_lowercase()))
        .unwrap_or_default()
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

    /// Sorte de piste : une ancienne session (sans le champ) n'a que des pistes
    /// d'enregistrement ; une piste audio est conservée telle quelle.
    #[test]
    fn track_kind_is_saved() {
        let json = r#"{"id":1,"name":"Voix","input":0,"stereo":false,"clips":[]}"#;
        let t: Track = serde_json::from_str(json).unwrap();
        assert_eq!(t.kind, TrackKind::Record);
        let mut p = Project::new("K".into(), std::env::temp_dir().join("tp-kind"), 48000);
        let id = p.add_audio_track("Musique".into(), true);
        let t = p.tracks.iter().find(|t| t.id == id).unwrap();
        assert!(t.is_audio() && t.stereo && !t.armed && t.input.is_none());
        let back: Track = serde_json::from_str(&serde_json::to_string(t).unwrap()).unwrap();
        assert!(back.is_audio());
    }

    /// Charge : session de 64 pistes × 500 prises (32 000 prises) : temps d'enregistrement
    /// et de réouverture du fichier de session.
    /// `cargo test --release -- --ignored --nocapture charge_session`
    #[test]
    #[ignore]
    fn charge_session_32000_prises() {
        let dir = std::env::temp_dir().join(format!("tp-charge-session-{}", std::process::id()));
        let mut p = Project::new("Charge".into(), dir.clone(), 48000);
        for t in 0..64u16 {
            let id = p.add_track(None, Some(t));
            for k in 0..500u64 {
                p.track_mut(id).unwrap().clips.push(Clip {
                    id: 1000 + t as u64 * 1000 + k,
                    file: format!("Piste {t}_{k:03}.wav"),
                    start: k * 48000 * 10,
                    offset: 0,
                    len: 48000 * 9,
                    src_len: 48000 * 9,
                    fade_in: 480,
                    fade_out: 480,
                });
            }
        }
        let t = std::time::Instant::now();
        p.save().unwrap();
        let save = t.elapsed();
        let size = std::fs::metadata(p.file_path()).unwrap().len();
        let t = std::time::Instant::now();
        let q = Project::load(&p.file_path()).unwrap();
        let load = t.elapsed();
        let t = std::time::Instant::now();
        let end = q.end();
        let end_t = t.elapsed();
        println!(
            "session 64 pistes × 500 prises : fichier {:.1} Mo, enregistrement {:?}, ouverture {:?}, calcul de la fin {:?}",
            size as f64 / 1e6,
            save,
            load,
            end_t
        );
        assert_eq!(
            q.tracks.iter().map(|t| t.clips.len()).sum::<usize>(),
            32_000
        );
        assert!(end > 0);
        std::fs::remove_dir_all(dir).ok();
    }

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

    fn audio_clip(id: u64, file: &str) -> Clip {
        Clip {
            id,
            file: file.into(),
            start: 0,
            offset: 0,
            len: 4,
            src_len: 4,
            fade_in: 0,
            fade_out: 0,
        }
    }

    #[test]
    fn save_as_never_reuses_or_overwrites() {
        let base = std::env::temp_dir().join(format!("tp-saveas-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let mut p = Project::new("Concert".into(), base.join("A"), 48000);
        p.ensure_dirs().unwrap();
        let t = p.add_track(None, Some(0));
        std::fs::write(p.audio_dir().join("Voix_001.wav"), b"nouveau").unwrap();
        std::fs::write(p.audio_dir().join("Voix_002.wav"), b"autre").unwrap();
        p.track_mut(t)
            .unwrap()
            .clips
            .push(audio_clip(50, "Voix_001.wav"));
        p.track_mut(t)
            .unwrap()
            .clips
            .push(audio_clip(51, "Voix_001.wav"));
        p.track_mut(t)
            .unwrap()
            .clips
            .push(audio_clip(52, "Voix_002.wav"));
        p.save().unwrap();

        // Cible : un ancien fichier du même nom (autre son) déjà présent.
        let target = base.join("B");
        std::fs::create_dir_all(target.join(AUDIO_DIR)).unwrap();
        std::fs::write(target.join(AUDIO_DIR).join("voix_001.wav"), b"ancien").unwrap();
        let mut q = p.clone();
        q.save_as(target.clone(), "Copie".into()).unwrap();
        assert_eq!(q.dir, target);
        let files: Vec<&str> = q.tracks[0].clips.iter().map(|c| c.file.as_str()).collect();
        assert_eq!(
            files,
            ["Voix_001 (2).wav", "Voix_001 (2).wav", "Voix_002.wav"]
        );
        let read = |f: &str| std::fs::read(target.join(AUDIO_DIR).join(f)).unwrap();
        assert_eq!(read("Voix_001 (2).wav"), b"nouveau");
        assert_eq!(read("voix_001.wav"), b"ancien", "fichier existant intact");
        assert_eq!(read("Voix_002.wav"), b"autre");
        // La session relue pointe vers les bons fichiers.
        let r = Project::load(&q.file_path()).unwrap();
        assert_eq!(r.tracks[0].clips[0].file, "Voix_001 (2).wav");

        // Une session du même nom existe déjà dans le dossier : refus, rien ne change.
        let mut again = p.clone();
        let e = again.save_as(target.clone(), "Copie".into()).unwrap_err();
        assert!(format!("{e:#}").contains("existe déjà"), "{e:#}");
        assert_eq!(again.dir, base.join("A"));
        // Réenregistrer au même endroit reste possible.
        q.save_as(target.clone(), "Copie".into()).unwrap();
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn load_repairs_incoherent_files() {
        let dir = std::env::temp_dir().join(format!("tp-validate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Vieux.tracksplus");
        // Fichier partiel : pas de nom, fréquence nulle, next_id déjà utilisé, fondu trop long.
        let json = r#"{"sample_rate":0,"next_id":3,"tracks":[{"id":7,"clips":[
            {"id":12,"file":"a.wav","len":100,"fade_in":500}]}]}"#;
        std::fs::write(&path, json).unwrap();
        let mut p = Project::load(&path).unwrap();
        assert_eq!(p.name, "Vieux");
        assert_eq!(p.sample_rate, DEFAULT_SAMPLE_RATE);
        assert_eq!(p.next_id, 13);
        assert_eq!(p.tracks[0].name, "Piste 1");
        assert!(p.tracks[0].input.is_none() && !p.tracks[0].armed);
        let c = &p.tracks[0].clips[0];
        assert_eq!((c.start, c.fade_in, c.fade_out), (0, 100, 0));
        assert!(p.alloc_id() > 12);
        // Fichier minimal.
        std::fs::write(&path, "{}").unwrap();
        let p = Project::load(&path).unwrap();
        assert_eq!((p.sample_rate, p.next_id, p.tracks.len()), (48000, 1, 0));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn atomic_write_replaces_whole_file() {
        let dir = std::env::temp_dir().join(format!("tp-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("sous").join("prefs.json");
        write_atomic(&path, b"premier contenu long").unwrap();
        write_atomic(&path, b"court").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"court");
        let left: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(left.len(), 1, "aucun fichier temporaire laissé");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn sanitize_names() {
        assert_eq!(sanitize("Voix/Lead:1"), "Voix_Lead_1");
        assert_eq!(sanitize("  "), "Piste");
    }

    #[test]
    fn clip_file_names_cannot_escape_audio_dir() {
        for (input, want) in [
            ("Voix_001.wav", "Voix_001.wav"),
            ("../../../Startup/x.bat", "x.bat"),
            ("..\\..\\x.wav", "x.wav"),
            ("C:\\Windows\\x.wav", "x.wav"),
            ("C:x.wav", "x.wav"),
            ("\\\\attaquant\\partage\\a.wav", "a.wav"),
            ("/etc/passwd", "passwd"),
            ("..", ""),
            ("dossier/", ""),
            ("", ""),
        ] {
            assert_eq!(safe_file_name(input), want, "{input}");
        }
        let mut p = Project::new("S".into(), std::env::temp_dir().join("tp-safe-name"), 48000);
        p.add_track(None, None);
        p.tracks[0].clips.push(audio_clip(50, "../../evil.wav"));
        p.validate();
        assert_eq!(p.tracks[0].clips[0].file, "evil.wav");
        assert_eq!(
            p.clip_path(&p.tracks[0].clips[0]),
            p.audio_dir().join("evil.wav")
        );
    }

    /// Sauvegarde automatique en arrière-plan : un état plus ancien n'écrase jamais un
    /// état plus récent, même s'il est écrit après.
    #[test]
    fn older_save_never_overwrites_newer() {
        let dir = std::env::temp_dir().join(format!("tp-seq-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut p = Project::new("Q".into(), dir.clone(), 48000);
        p.add_track(Some("Ancienne".into()), None);
        let old_seq = next_save_seq();
        let old = p.clone();
        p.tracks[0].name = "Récente".into();
        p.save().unwrap();
        old.write_file(old_seq).unwrap();
        assert_eq!(
            Project::load(&p.file_path()).unwrap().tracks[0].name,
            "Récente"
        );
        // Sauvegarde en arrière-plan normale : bien écrite.
        p.tracks[0].name = "Auto".into();
        p.save_in_background().recv().unwrap().unwrap();
        assert!(!p.dirty);
        assert_eq!(
            Project::load(&p.file_path()).unwrap().tracks[0].name,
            "Auto"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    /// Annuler un import : la piste audio créée par l'import disparaît ; une piste
    /// d'enregistrement créée depuis reste, sans prise.
    #[test]
    fn undo_removes_imported_audio_track() {
        let mut p = Project::new("U".into(), std::env::temp_dir().join("tp-undo-imp"), 48000);
        p.add_track(Some("Voix".into()), Some(0));
        let snap = p.tracks.clone();
        let rec = p.add_track(Some("Basse".into()), Some(1));
        let audio = p.add_audio_track("Musique".into(), true);
        let id = p.alloc_id();
        p.track_mut(audio)
            .unwrap()
            .clips
            .push(audio_clip(id, "m.wav"));
        let merged = merge_clips(p.tracks.clone(), snap);
        assert!(merged.iter().all(|t| t.id != audio));
        let basse = merged.iter().find(|t| t.id == rec).unwrap();
        assert!(basse.clips.is_empty() && basse.input == Some(1));
    }
}
