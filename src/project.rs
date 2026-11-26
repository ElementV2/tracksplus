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
    /// Valeur inconnue (version plus récente) : piste d'enregistrement, session protégée.
    #[serde(default, deserialize_with = "lenient")]
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
    /// Fader de la piste dans le mix (dB, `FADER_MIN_DB` = coupé) : écoute et mix exporté
    /// seulement, les prises ne sont jamais modifiées.
    #[serde(default, deserialize_with = "lenient_f32")]
    pub gain_db: f32,
    /// Panoramique (−1 = gauche, 0 = centre, 1 = droite). Piste mono : loi à −3 dB au
    /// centre (comme une console) ; piste stéréo : balance.
    #[serde(default, deserialize_with = "lenient_f32")]
    pub pan: f32,
    /// Les prises récentes sont à la fin et passent au-dessus des anciennes.
    #[serde(default)]
    pub clips: Vec<Clip>,
    /// Champs inconnus (écrits par une version plus récente) : réécrits tels quels.
    #[serde(flatten)]
    pub extra: Extra,
}

impl Track {
    pub fn is_audio(&self) -> bool {
        self.kind == TrackKind::Audio
    }

    /// Nombre de canaux de la piste (1 = mono, 2 = stéréo).
    pub fn channels(&self) -> usize {
        if self.stereo { 2 } else { 1 }
    }

    /// Gains linéaires (gauche, droite) de la piste dans le mix : fader et panoramique.
    pub fn mix_gains(&self) -> [f32; 2] {
        let g = db_to_gain(self.gain_db);
        let pan = if self.pan.is_finite() {
            self.pan.clamp(-1.0, 1.0)
        } else {
            0.0
        };
        if self.stereo {
            // Balance : le côté opposé baisse, l'autre reste tel quel.
            [g * (1.0 - pan).min(1.0), g * (1.0 + pan).min(1.0)]
        } else {
            // Puissance constante : −3 dB de chaque côté au centre.
            let a = (pan + 1.0) * std::f32::consts::FRAC_PI_4;
            [g * a.cos(), g * a.sin()]
        }
    }

    /// Canaux de la carte utilisés par la piste.
    pub fn inputs(&self) -> impl Iterator<Item = u16> + '_ {
        self.input
            .into_iter()
            .flat_map(move |i| (0..self.channels() as u16).map(move |k| i.saturating_add(k)))
    }

    /// L'entrée (numéro réel sur la carte, à partir de 0 : « IN 7 » = 6) vue par le flux
    /// ouvert, qui commence au canal `base` de la carte (WASAPI : Windows découpe la carte
    /// en paires, « Analog (7+8) » commence à 6 ; ASIO : 0). `None` hors de ce flux.
    pub fn engine_input(&self, base: u16) -> Option<u16> {
        self.input.and_then(|i| i.checked_sub(base))
    }

    /// L'entrée (ou la paire) existe dans le flux ouvert (`in_ch` canaux à partir de `base`).
    pub fn input_fits(&self, base: u16, in_ch: u16) -> bool {
        self.engine_input(base)
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
    /// Champs inconnus, réécrits tels quels.
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Project {
    /// Version du format du fichier (toujours écrite à `FORMAT_VERSION`) : un fichier d'une
    /// version plus récente de l'application s'ouvre en lecture seule.
    #[serde(default, serialize_with = "write_format_version")]
    pub format_version: u32,
    /// Nom (remplacé au chargement par celui du fichier).
    #[serde(default)]
    pub name: String,
    #[serde(default = "default_sample_rate")]
    pub sample_rate: u32,
    #[serde(default, deserialize_with = "lenient")]
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
    /// Champs inconnus (version plus récente) : réécrits tels quels.
    #[serde(flatten)]
    pub extra: Extra,
    /// Session à ne pas écraser (fichier d'une version plus récente…) : la raison, donnée
    /// quand une sauvegarde est refusée. « Enregistrer sous » reste possible.
    #[serde(skip)]
    pub read_only: Option<String>,
    /// Le fichier de session est le nôtre (lu ou écrit par cette session) : une session
    /// neuve n'écrase jamais un fichier du même nom déjà présent sur le disque.
    #[serde(skip)]
    owns_file: bool,
    /// Moment de l'état enregistré (millisecondes depuis 1970, horloge de l'ordinateur) :
    /// départage les versions de secours à l'ouverture d'un fichier abîmé (voir `open`).
    #[serde(default)]
    pub saved_at: i64,
    /// Moment de l'état copié, s'il a été figé avant la sauvegarde (double de la session
    /// écrit plus tard par un thread : il garde l'heure de son contenu, pas celle de son
    /// écriture).
    #[serde(skip)]
    pub state_at: Option<i64>,
    /// Fader master du mix (dB, `FADER_MIN_DB` = coupé) : écoute et mix exporté.
    #[serde(default, deserialize_with = "lenient_f32")]
    pub master_db: f32,
    /// Champs inconnus des prises (version plus récente), par identifiant de prise :
    /// réécrits tels quels (voir `parse_project`, `to_json`). Rangés à part pour que les
    /// prises restent de simples structures.
    #[serde(skip)]
    clip_extra: std::collections::HashMap<u64, Extra>,
}

/// Faders : de `FADER_MIN_DB` (coupé) à `FADER_MAX_DB`.
pub const FADER_MIN_DB: f32 = -90.0;
pub const FADER_MAX_DB: f32 = 6.0;

/// dB → gain linéaire ; `FADER_MIN_DB` et en dessous : silence.
pub fn db_to_gain(db: f32) -> f32 {
    if !db.is_finite() || db <= FADER_MIN_DB {
        0.0
    } else {
        10f32.powf(db.min(FADER_MAX_DB) / 20.0)
    }
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
            format_version: FORMAT_VERSION,
            extra: Extra::new(),
            read_only: None,
            owns_file: false,
            saved_at: 0,
            state_at: None,
            master_db: 0.0,
            clip_extra: Default::default(),
        }
    }

    /// Nouvelle session horodatée dans le dossier par défaut, dans un dossier qui n'existe
    /// pas encore (« … (2) », « … (3) »… sinon) : jamais celui d'une session existante.
    pub fn untitled(sample_rate: u32) -> Self {
        let base = chrono::Local::now()
            .format("Session %Y-%m-%d %Hh%M")
            .to_string();
        let root = default_sessions_root();
        let name = free_dir_name(&root, &base);
        let dir = root.join(&name);
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
            gain_db: 0.0,
            pan: 0.0,
            clips: Vec::new(),
            extra: Extra::new(),
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

    /// Gain linéaire du fader master.
    pub fn master_gain(&self) -> f32 {
        db_to_gain(self.master_db)
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
            extra: Extra::new(),
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
        // Champs inconnus (version plus récente) : valables pour les deux moitiés.
        if let Some(e) = self.clip_extra.get(&clip_id).cloned() {
            self.clip_extra.insert(new_id, e);
        }
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

    /// Sauvegarde, attendue `SAVE_WAIT` au plus : un disque bloqué (NAS, disque USB en
    /// veille) ne fige jamais l'interface. Après ce délai, l'écriture continue en arrière-plan
    /// et la session reste « à sauvegarder ».
    pub fn save(&mut self) -> Result<()> {
        self.save_waiting(false)
    }

    /// Comme `save`, en gardant à coup sûr la version précédente du fichier parmi les copies
    /// `.bak` (fin de prise : l'état d'avant la prise reste récupérable).
    pub fn save_checkpoint(&mut self) -> Result<()> {
        self.save_waiting(true)
    }

    fn save_waiting(&mut self, checkpoint: bool) -> Result<()> {
        let rx = self.start_write(checkpoint)?;
        let res = match rx.recv_timeout(SAVE_WAIT) {
            Ok(r) => r,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(anyhow::anyhow!(
                "disque lent : sauvegarde en cours depuis {} s",
                SAVE_WAIT.as_secs()
            )),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                Err(anyhow::anyhow!("sauvegarde interrompue"))
            }
        };
        if res.is_err() {
            self.dirty = true;
        }
        res
    }

    /// Sauvegarde dans un thread (sauvegarde automatique : un disque lent ne fige pas
    /// l'interface). L'état est figé maintenant ; si une sauvegarde plus récente a déjà été
    /// écrite entre-temps, celle-ci est abandonnée (jamais d'ancien état par-dessus).
    pub fn save_in_background(&mut self) -> std::sync::mpsc::Receiver<Result<()>> {
        match self.start_write(false) {
            Ok(rx) => rx,
            Err(e) => {
                let (tx, rx) = std::sync::mpsc::channel();
                let _ = tx.send(Err(e));
                rx
            }
        }
    }

    /// Lance l'écriture du fichier de session dans un thread : refusée tout de suite pour
    /// une session en lecture seule, ou pour une session neuve dont le fichier existe déjà
    /// (ce serait celui d'une autre session).
    fn start_write(&mut self, checkpoint: bool) -> Result<std::sync::mpsc::Receiver<Result<()>>> {
        if let Some(why) = self.save_refused() {
            bail!("{why}");
        }
        self.owns_file = true;
        let seq = next_save_seq();
        let mut snapshot = self.clone();
        snapshot.saved_at = self.state_at.take().unwrap_or_else(now_ms);
        self.dirty = false;
        let (tx, rx) = std::sync::mpsc::channel();
        let job_tx = tx.clone();
        let spawned = std::thread::Builder::new()
            .name("tracksplus-save".into())
            .spawn(move || {
                let _ = job_tx.send(snapshot.write_file(seq, checkpoint));
            });
        if spawned.is_err() {
            // Pas de thread disponible : écriture directe.
            let _ = tx.send(self.write_file(seq, checkpoint));
        }
        Ok(rx)
    }

    /// Raison pour laquelle toute sauvegarde de la session serait refusée (et le resterait) :
    /// session en lecture seule, ou session neuve dont le fichier existe déjà (ce serait
    /// celui d'une autre session). Une prise est alors refusée : elle ne serait écrite nulle
    /// part dans la session.
    pub fn save_refused(&self) -> Option<String> {
        if let Some(why) = &self.read_only {
            return Some(why.clone());
        }
        if !self.owns_file && self.file_path().try_exists().unwrap_or(true) {
            return Some(format!("« {} » existe déjà ici", self.name));
        }
        None
    }

    /// Fige l'heure de l'état actuel : la prochaine sauvegarde de cette copie (double de la
    /// session, écrit plus tard) la note comme moment de son contenu.
    pub fn mark_state_time(&mut self) {
        self.state_at = Some(now_ms());
    }

    /// Texte JSON du fichier de session, champs inconnus des prises compris.
    fn to_json(&self) -> serde_json::Result<String> {
        if self.clip_extra.is_empty() {
            return serde_json::to_string_pretty(self);
        }
        let mut v = serde_json::to_value(self)?;
        let clips = v
            .get_mut("tracks")
            .and_then(|t| t.as_array_mut())
            .into_iter()
            .flatten()
            .filter_map(|t| t.get_mut("clips").and_then(|c| c.as_array_mut()))
            .flatten();
        for c in clips {
            let Some(obj) = c.as_object_mut() else {
                continue;
            };
            let Some(extra) = obj
                .get("id")
                .and_then(|i| i.as_u64())
                .and_then(|id| self.clip_extra.get(&id))
            else {
                continue;
            };
            for (k, val) in extra {
                // Un champ connu n'est jamais remplacé par un champ mis de côté.
                obj.entry(k.clone()).or_insert_with(|| val.clone());
            }
        }
        serde_json::to_string_pretty(&v)
    }

    /// Sauvegarde préparée maintenant, écrite plus tard (fin de session Windows) : l'état
    /// et son rang sont figés tout de suite ; si une sauvegarde plus récente a été écrite
    /// entre-temps, l'écriture est abandonnée. Ne touche pas à `dirty`.
    pub fn deferred_save(&self) -> impl FnOnce() -> Result<()> + Send + 'static {
        let refused = self.save_refused();
        let seq = next_save_seq();
        let mut snapshot = self.clone();
        snapshot.saved_at = self.state_at.unwrap_or_else(now_ms);
        move || match refused {
            // Session en lecture seule ou fichier d'une autre session : jamais écrasé.
            Some(why) => Err(anyhow::anyhow!(why)),
            // Point de reprise : la version remplacée rejoint les copies `.bak`.
            None => snapshot.write_file(seq, true),
        }
    }

    /// Écriture atomique du fichier de session (fichier temporaire synchronisé puis
    /// renommage), une à la fois, et seulement si rien de plus récent n'a été écrit. La
    /// version remplacée rejoint les copies `.bak` (toutes les 5 min, ou `checkpoint`).
    fn write_file(&self, seq: u64, checkpoint: bool) -> Result<()> {
        use std::sync::TryLockError;
        let path = self.file_path();
        let lock = file_lock(&path);
        // Attente bornée : une écriture précédente bloquée sur un disque figé ne retient
        // pas celle-ci indéfiniment.
        let deadline = std::time::Instant::now() + LOCK_WAIT;
        let mut last = loop {
            match lock.try_lock() {
                Ok(g) => break g,
                Err(TryLockError::Poisoned(e)) => break e.into_inner(),
                Err(TryLockError::WouldBlock) => {
                    if std::time::Instant::now() >= deadline {
                        bail!("disque occupé : sauvegarde précédente en cours");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
        };
        if *last > seq {
            return Ok(());
        }
        self.ensure_dirs()?;
        let json = self.to_json()?;
        rotate_backups(&path, checkpoint);
        write_atomic(&path, json.as_bytes())?;
        *last = seq;
        Ok(())
    }

    /// Lecture d'un fichier de session, sans rien modifier sur le disque (écran d'accueil,
    /// tests). Voir `open` pour l'ouverture par l'utilisateur.
    pub fn load(path: &Path) -> Result<Self> {
        let json = read_retrying(path).with_context(|| format!("Lecture de {}", path.display()))?;
        Self::from_json(&json, path)
    }

    /// Session lue depuis le texte `json`, rangée à l'emplacement `path`.
    fn from_json(json: &str, path: &Path) -> Result<Self> {
        let (mut p, unknown) = match parse_project(json) {
            Ok(r) => r,
            Err(e) => {
                if let Some(v) = newer_format(json) {
                    bail!(
                        "Session d'une version plus récente (format {v}) · mettez à jour TracksPlus"
                    );
                }
                return Err(anyhow::Error::new(e).context("Fichier de session invalide"));
            }
        };
        p.dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        p.name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or(p.name);
        p.validate();
        p.dirty = false;
        p.owns_file = true;
        if p.format_version > FORMAT_VERSION || unknown {
            p.read_only = Some("session d'une version plus récente".into());
        }
        Ok(p)
    }

    /// Ouverture d'une session par l'utilisateur. Si le fichier est abîmé (illisible), la
    /// version de secours la plus récente qui se lit est reprise, parmi les copies `.bak` et
    /// `fallbacks` (par exemple le double du second disque et ses copies) : la plus récente
    /// d'après l'heure de son contenu (`saved_at`), puis la date du fichier. Le fichier abîmé
    /// est d'abord copié de côté sous un autre nom, puis remplacé d'un coup par la version
    /// reprise : il y a toujours un fichier de session à sa place (accueil, récupération au
    /// lancement). Renvoie aussi le message à afficher.
    pub fn open(path: &Path, fallbacks: &[PathBuf]) -> Result<(Self, Option<String>)> {
        // Lecture impossible (fichier verrouillé, disque absent) : pas de repli, le fichier
        // est peut-être intact.
        let json = read_retrying(path).with_context(|| format!("Lecture de {}", path.display()))?;
        clean_stale_temps(path);
        let err = match Self::from_json(&json, path) {
            Ok(p) => return Ok((p, None)),
            Err(e) => e,
        };
        if newer_format(&json).is_some() {
            return Err(err);
        }
        let candidates = backup_paths(path)
            .into_iter()
            .chain(fallbacks.iter().cloned());
        let Some(best) = newest_readable(path, candidates) else {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            return Err(err.context(format!(
                "« {name} » illisible, aucune version précédente lisible"
            )));
        };
        let Candidate {
            path: from,
            text,
            project: mut p,
        } = best;
        let from = from.display();
        let read_only_msg =
            || format!("Session illisible · version précédente en lecture seule ({from})");
        if p.read_only.is_some() {
            // Version de secours d'un format plus récent : rien n'est touché sur le disque.
            return Ok((p, Some(read_only_msg())));
        }
        let aside = match copy_aside(path, "illisible") {
            Ok(a) => a,
            Err(e) => {
                p.read_only = Some(format!("fichier abîmé non mis de côté : {e}"));
                return Ok((p, Some(read_only_msg())));
            }
        };
        let aside = aside.file_name().unwrap_or_default().to_string_lossy();
        // Remplacement d'un coup : à sa place, le fichier abîmé (déjà copié de côté) ou la
        // version reprise, jamais aucun des deux.
        let restored = write_atomic(path, text.as_bytes());
        // Fichier en place resté abîmé : la prochaine sauvegarde le remplacera.
        p.dirty = restored.is_err();
        let mut msg = format!(
            "Session illisible · version précédente reprise ({from}) · ancien fichier : « {aside} »"
        );
        if let Err(e) = restored {
            msg += &format!(" · non réécrite : {e:#}");
        }
        Ok((p, Some(msg)))
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
        // Les champs inconnus des prises suivent leur prise.
        let mut old_extra = std::mem::take(&mut self.clip_extra);
        for t in &mut self.tracks {
            t.id = alloc();
            for c in &mut t.clips {
                let old = c.id;
                c.id = alloc();
                if let Some(e) = old_extra.remove(&old) {
                    self.clip_extra.insert(c.id, e);
                }
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
            bail!("« {} » existe déjà dans ce dossier", next.name);
        }
        if target != self.file_path() {
            // Copie ailleurs : le fichier protégé (version plus récente) reste intact, la
            // copie est une session à part entière, modifiable.
            next.read_only = None;
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
        self.new_take_file_in(track_name, &self.audio_file_names(), taken)
    }

    /// Noms des fichiers du dossier Audio, en minuscules (une seule lecture du dossier,
    /// partagée par toutes les pistes d'une prise : 64 pistes = 1 lecture, pas 64).
    pub fn audio_file_names(&self) -> HashSet<String> {
        std::fs::read_dir(self.audio_dir())
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().to_lowercase())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Comme `new_take_file_avoiding`, avec les noms déjà lus (`audio_file_names`).
    pub fn new_take_file_in(
        &self,
        track_name: &str,
        existing: &HashSet<String>,
        taken: &HashSet<String>,
    ) -> String {
        let base = sanitize(track_name);
        for n in 1.. {
            let f = format!("{base}_{n:03}.wav");
            let low = f.to_lowercase();
            let p2 = format!("{base}_{n:03}_p2.wav").to_lowercase();
            // `taken` est en minuscules ; un nom laissé dans sa casse d'origine est reconnu
            // quand même.
            if !existing.contains(&low)
                && !existing.contains(&p2)
                && !taken.contains(&low)
                && !taken.contains(&f)
            {
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
/// `snap_next_id` : compteur d'identifiants au moment de l'instantané. Une piste absente
/// de l'instantané mais plus ancienne que lui y avait été supprimée : elle disparaît
/// (Rétablir une suppression la refait vraiment).
pub fn merge_clips(mut current: Vec<Track>, snap: Vec<Track>, snap_next_id: u64) -> Vec<Track> {
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
        if t.is_audio() || t.id < snap_next_id {
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

/// Nom temporaire unique voisin de `path` (`nom.ext.<pid>-<n>.<suffixe>`) : deux écritures
/// simultanées (session « Soundboard » et `soundboard.json`, double de la session, autre
/// instance) n'utilisent jamais le même fichier temporaire.
pub fn temp_sibling(path: &Path, suffix: &str) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}-{n}.{suffix}", std::process::id()));
    path.with_file_name(name)
}

/// Écriture atomique : fichier temporaire voisin (nom unique) forcé sur le disque, puis
/// renommé (avec nouveaux essais, voir `replace_file`). Une coupure en pleine écriture laisse
/// l'ancien fichier intact, jamais un fichier tronqué ; aucun temporaire n'est laissé.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(d) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(d).with_context(|| format!("Création de {}", d.display()))?;
    }
    let tmp = temp_sibling(path, "tmp");
    if let Err(e) = write_synced(&tmp, bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("Écriture de {}", tmp.display()));
    }
    if let Err(e) = replace_file(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("Écriture de {}", path.display()));
    }
    Ok(())
}

/// Copie `src` vers `dst` via un fichier temporaire forcé sur le disque, puis renommé.
pub fn copy_synced(src: &Path, dst: &Path) -> Result<()> {
    let tmp = temp_sibling(dst, "copie");
    let res = (|| -> Result<()> {
        std::fs::copy(src, &tmp).with_context(|| format!("Copie de {}", src.display()))?;
        // Accès en écriture : exigé par Windows pour forcer l'écriture sur le disque.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&tmp)?
            .sync_all()?;
        replace_file(&tmp, dst)?;
        Ok(())
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

/// Erreur passagère sous Windows : fichier ouvert un instant par l'antivirus, OneDrive ou
/// l'indexation (accès refusé, violation de partage ou de verrou).
fn transient(e: &std::io::Error) -> bool {
    // ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION.
    let windows = cfg!(windows) && matches!(e.raw_os_error(), Some(5 | 32 | 33));
    windows || e.kind() == std::io::ErrorKind::PermissionDenied
}

/// Refait `op` après une erreur passagère : 8 essais, pauses de 20, 40, 80… ms (300 ms au
/// plus), environ 1,5 s en tout.
fn retrying<T>(mut op: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    let mut pause = 20;
    for _ in 0..7 {
        match op() {
            Err(e) if transient(&e) => {
                std::thread::sleep(std::time::Duration::from_millis(pause));
                pause = (pause * 2).min(300);
            }
            r => return r,
        }
    }
    op()
}

/// Lecture d'un fichier texte, avec nouveaux essais sur erreur passagère.
pub fn read_retrying(path: &Path) -> std::io::Result<String> {
    retrying(|| std::fs::read_to_string(path))
}

/// Remplace `dst` par `src` (renommage), avec nouveaux essais sur erreur passagère. Sous
/// Windows, le renommage est écrit sur le disque avant de rendre la main (une coupure juste
/// après ne le défait pas).
pub fn replace_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    retrying(|| move_file(src, dst, true))
}

/// Renomme `src` en `dst` sans jamais remplacer un fichier existant (erreur
/// `AlreadyExists`).
pub fn rename_no_replace(src: &Path, dst: &Path) -> std::io::Result<()> {
    retrying(|| move_file(src, dst, false))
}

#[cfg(windows)]
fn move_file(src: &Path, dst: &Path, replace: bool) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    // Chemin absolu, en forme longue (`\\?\`) au-delà de la limite classique de 260
    // caractères.
    fn wide(p: &Path) -> std::io::Result<Vec<u16>> {
        let abs = std::path::absolute(p)?;
        let raw: Vec<u16> = abs.as_os_str().encode_wide().collect();
        let starts = |pre: &str| raw.starts_with(&pre.encode_utf16().collect::<Vec<_>>());
        let mut v: Vec<u16> = if raw.len() < 240 || starts(r"\\?\") {
            raw.clone()
        } else if starts(r"\\") {
            r"\\?\UNC"
                .encode_utf16()
                .chain(raw[1..].iter().copied())
                .collect()
        } else {
            r"\\?\".encode_utf16().chain(raw.iter().copied()).collect()
        };
        v.push(0);
        Ok(v)
    }
    let (s, d) = (wide(src)?, wide(dst)?);
    let mut flags = MOVEFILE_WRITE_THROUGH;
    if replace {
        flags |= MOVEFILE_REPLACE_EXISTING;
    }
    // SAFETY : deux chaînes UTF-16 terminées par zéro, vivantes pendant l'appel.
    if unsafe { MoveFileExW(s.as_ptr(), d.as_ptr(), flags) } != 0 {
        return Ok(());
    }
    let e = std::io::Error::last_os_error();
    if replace && e.kind() != std::io::ErrorKind::NotFound {
        // Cible ouverte par un autre programme (antivirus, synchronisation) avec partage de
        // suppression : le renommage de la bibliothèque standard (sémantique POSIX) la
        // remplace quand même.
        return std::fs::rename(src, dst);
    }
    Err(e)
}

#[cfg(not(windows))]
fn move_file(src: &Path, dst: &Path, replace: bool) -> std::io::Result<()> {
    if replace {
        std::fs::rename(src, dst)?;
    } else {
        // Un lien dur échoue de lui-même si la cible existe (pas de course entre test et
        // renommage) ; système de fichiers sans liens durs : test puis renommage.
        match std::fs::hard_link(src, dst) {
            Ok(()) => std::fs::remove_file(src)?,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Err(e),
            Err(_) => {
                if dst.try_exists()? {
                    return Err(std::io::ErrorKind::AlreadyExists.into());
                }
                std::fs::rename(src, dst)?;
            }
        }
    }
    // Renommage écrit sur le disque (entrée du dossier).
    if let Some(d) = dst.parent().filter(|d| !d.as_os_str().is_empty()) {
        let _ = std::fs::File::open(d).and_then(|f| f.sync_all());
    }
    Ok(())
}

/// Met un fichier de côté sans rien écraser : `nom.ext.<motif>-AAAAMMJJ-HHMMSS` (ou
/// `nom.<motif>-AAAAMMJJ-HHMMSS.ext` avec `keep_ext`), suivi de `-2`, `-3`… si ce nom est
/// pris. Renvoie le nouveau chemin.
pub fn set_aside(path: &Path, tag: &str, keep_ext: bool) -> std::io::Result<PathBuf> {
    place_aside(path, tag, keep_ext, |dst| rename_no_replace(path, dst))
}

/// Comme `set_aside`, mais en copiant : le fichier reste en place (copie forcée sur le
/// disque, jamais par-dessus un fichier existant).
pub fn copy_aside(path: &Path, tag: &str) -> std::io::Result<PathBuf> {
    place_aside(path, tag, false, |dst| {
        let tmp = temp_sibling(dst, "copie");
        let res = (|| {
            retrying(|| std::fs::copy(path, &tmp))?;
            // Accès en écriture : exigé par Windows pour forcer l'écriture sur le disque.
            std::fs::OpenOptions::new()
                .write(true)
                .open(&tmp)?
                .sync_all()?;
            rename_no_replace(&tmp, dst)
        })();
        if res.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        res
    })
}

/// Premier nom libre de mise de côté (voir `set_aside`) sur lequel `place` réussit.
fn place_aside(
    path: &Path,
    tag: &str,
    keep_ext: bool,
    mut place: impl FnMut(&Path) -> std::io::Result<()>,
) -> std::io::Result<PathBuf> {
    let date = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let file = path.file_name().unwrap_or_default().to_string_lossy();
    let (stem, ext) = match (keep_ext, file.rsplit_once('.')) {
        (true, Some((s, e))) => (s.to_string(), format!(".{e}")),
        _ => (file.to_string(), String::new()),
    };
    let mut last = std::io::Error::from(std::io::ErrorKind::AlreadyExists);
    for n in 1..100 {
        let extra = if n > 1 {
            format!("-{n}")
        } else {
            String::new()
        };
        let dst = path.with_file_name(format!("{stem}.{tag}-{date}{extra}{ext}"));
        match place(&dst) {
            Ok(()) => return Ok(dst),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = e,
            Err(e) => return Err(e),
        }
    }
    Err(last)
}

/// Version de secours lisible (voir `newest_readable`).
struct Candidate {
    path: PathBuf,
    text: String,
    project: Project,
}

/// Parmi `candidates`, la version lisible la plus récente, rangée à l'emplacement `path` :
/// d'après l'heure de son contenu (`saved_at`), puis la date de modification du fichier
/// (fichiers d'une version qui ne notait pas l'heure). Après une prise, la copie `.bak1`
/// est plus ancienne que le double du second disque ; plus tard, ce peut être l'inverse.
fn newest_readable(path: &Path, candidates: impl Iterator<Item = PathBuf>) -> Option<Candidate> {
    let mut best: Option<((bool, i64, std::time::SystemTime), Candidate)> = None;
    for cand in candidates {
        let Ok(text) = read_retrying(&cand) else {
            continue;
        };
        let Ok(project) = Project::from_json(&text, path) else {
            continue;
        };
        let mtime = std::fs::metadata(&cand)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        // Version venue d'ailleurs (double sur le second disque) : ses prises peuvent citer
        // des fichiers propres à ce disque (trou d'écriture d'un seul côté). Préférée
        // seulement si tous ses fichiers sont dans le dossier Audio de la session.
        let local = cand.parent() == path.parent();
        let complete = local
            || project
                .tracks
                .iter()
                .flat_map(|t| &t.clips)
                .all(|c| project.audio_dir().join(&c.file).is_file());
        let key = (complete, project.saved_at, mtime);
        if best.as_ref().is_none_or(|(k, _)| key > *k) {
            best = Some((
                key,
                Candidate {
                    path: cand,
                    text,
                    project,
                },
            ));
        }
    }
    best.map(|(_, c)| c)
}

/// Nombre de versions précédentes gardées (`.bak1` la plus récente … `.bak3`).
pub const BACKUPS: usize = 3;
/// Une nouvelle version précédente au plus toutes les 5 min (hors fins de prise).
const BACKUP_EVERY: std::time::Duration = std::time::Duration::from_secs(300);
/// Attente d'une sauvegarde par l'interface (voir `Project::save`).
pub const SAVE_WAIT: std::time::Duration = std::time::Duration::from_secs(8);
/// Attente de la fin d'une écriture précédente du même fichier.
const LOCK_WAIT: std::time::Duration = if cfg!(test) {
    std::time::Duration::from_millis(300)
} else {
    std::time::Duration::from_secs(5)
};

/// Copies des versions précédentes d'un fichier de session, la plus récente en premier.
pub fn backup_paths(path: &Path) -> Vec<PathBuf> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    (1..=BACKUPS)
        .map(|i| path.with_file_name(format!("{name}.bak{i}")))
        .collect()
}

/// Avant de remplacer `path` : sa version actuelle rejoint les copies `.bak` (rotation),
/// si la dernière copie a plus de 5 min ou si `force`. Un fichier en place devenu illisible
/// n'est jamais remplacé en silence : il est mis de côté. Au mieux : un échec ici
/// n'empêche jamais la sauvegarde.
fn rotate_backups(path: &Path, force: bool) {
    let baks = backup_paths(path);
    let recent = std::fs::metadata(&baks[0])
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < BACKUP_EVERY);
    if recent && !force {
        return;
    }
    let Ok(current) = retrying(|| std::fs::read(path)) else {
        return;
    };
    if serde_json::from_slice::<serde::de::IgnoredAny>(&current).is_err() {
        let _ = set_aside(path, "illisible", false);
        return;
    }
    for i in (1..BACKUPS).rev() {
        let _ = replace_file(&baks[i - 1], &baks[i]);
    }
    let _ = write_atomic(&baks[0], &current);
}

/// Fichiers temporaires de `path` laissés par une écriture interrompue (crash, coupure)
/// depuis plus de 10 min : supprimés.
pub fn clean_stale_temps(path: &Path) {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return;
    };
    let prefix = format!("{}.", name.to_string_lossy());
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        let temp = n.starts_with(&prefix) && (n.ends_with(".tmp") || n.ends_with(".copie"));
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > std::time::Duration::from_secs(600));
        if temp && old {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Premier nom libre dans `root` : `base`, puis `base (2)`, `base (3)`…
fn free_dir_name(root: &Path, base: &str) -> String {
    (1..1000)
        .map(|n| {
            if n == 1 {
                base.to_string()
            } else {
                format!("{base} ({n})")
            }
        })
        .find(|name| !root.join(name).try_exists().unwrap_or(true))
        .unwrap_or_else(|| format!("{base} ({})", std::process::id()))
}

// ------------------------------------------------------------- format des fichiers

/// Version du format des fichiers de session et de soundboard. Une version précédente de
/// l'application ouvre en lecture seule un fichier d'un format plus récent, au lieu de le
/// réécrire en perdant des données.
///
/// Un champ ajouté (session, piste, prise, repère, soundboard, page, pad) n'impose pas de
/// l'augmenter : les champs inconnus sont relus et réécrits tels quels (`Extra`, et
/// `clip_extra` pour les prises). Une nouvelle valeur d'un champ existant est protégée par
/// `lenient`. À augmenter quand un fichier ne serait plus correct sans ce que la version
/// précédente ignore (nouveau sens d'un champ existant, nouvel objet hors de ces structures,
/// champ du journal des prises…).
pub const FORMAT_VERSION: u32 = 1;

/// Champs inconnus d'un objet (version plus récente), réécrits tels quels.
pub type Extra = serde_json::Map<String, serde_json::Value>;

/// Écrit toujours la version de format de cette application.
pub fn write_format_version<S: serde::Serializer>(_: &u32, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_u32(FORMAT_VERSION)
}

thread_local! {
    /// Une valeur inconnue a été remplacée par sa valeur par défaut pendant la lecture en
    /// cours (voir `lenient`, `parse_json`).
    static UNKNOWN_VALUE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Champ tolérant : une valeur inconnue (nouvelle variante d'une version plus récente…)
/// prend la valeur par défaut au lieu de rendre tout le fichier illisible, et le fichier
/// est signalé (`parse_json`) pour ne pas être réécrit sans elle.
pub fn lenient<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned + Default,
{
    let v = serde_json::Value::deserialize(d)?;
    Ok(serde_json::from_value(v).unwrap_or_else(|_| {
        UNKNOWN_VALUE.with(|u| u.set(true));
        T::default()
    }))
}

/// Nombre tolérant : `null` (NaN écrit par serde_json) ou valeur illisible → 0.
pub fn lenient_f64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    Ok(v.as_f64().filter(|x| x.is_finite()).unwrap_or(0.0))
}

/// Comme `lenient_f64`, en `f32` (hors limites → 0).
pub fn lenient_f32<'de, D: serde::Deserializer<'de>>(d: D) -> Result<f32, D::Error> {
    let x = lenient_f64(d)? as f32;
    Ok(if x.is_finite() { x } else { 0.0 })
}

/// Lecture JSON : la valeur, et `true` si des valeurs inconnues y ont été remplacées par
/// défaut (fichier d'une version plus récente : à ne pas réécrire).
pub fn parse_json<T: serde::de::DeserializeOwned>(s: &str) -> serde_json::Result<(T, bool)> {
    UNKNOWN_VALUE.with(|u| u.set(false));
    let r = serde_json::from_str(s);
    let unknown = UNKNOWN_VALUE.with(|u| u.replace(false));
    r.map(|v| (v, unknown))
}

/// Comme `parse_json`, depuis une valeur JSON déjà lue.
pub fn parse_value<T: serde::de::DeserializeOwned>(
    v: &serde_json::Value,
) -> serde_json::Result<(T, bool)> {
    UNKNOWN_VALUE.with(|u| u.set(false));
    let r = T::deserialize(v);
    let unknown = UNKNOWN_VALUE.with(|u| u.replace(false));
    r.map(|v| (v, unknown))
}

/// Champs connus d'une prise (`Clip`) : les autres sont gardés à part (`clip_extra`).
const CLIP_FIELDS: &[&str] = &[
    "id", "file", "start", "offset", "len", "src_len", "fade_in", "fade_out",
];

/// Lecture d'un fichier de session (voir `parse_json`), en gardant les champs inconnus des
/// prises, par identifiant de prise (avant toute correction de `validate`).
fn parse_project(json: &str) -> serde_json::Result<(Project, bool)> {
    let v: serde_json::Value = serde_json::from_str(json)?;
    let (mut p, unknown) = parse_value::<Project>(&v)?;
    let raw_tracks = v
        .get("tracks")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    for (t, raw) in p.tracks.iter().zip(raw_tracks) {
        let raw_clips = raw
            .get("clips")
            .and_then(serde_json::Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        for (c, rc) in t.clips.iter().zip(raw_clips) {
            let Some(obj) = rc.as_object() else {
                continue;
            };
            let extra: Extra = obj
                .iter()
                .filter(|(k, _)| !CLIP_FIELDS.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            if !extra.is_empty() {
                p.clip_extra.insert(c.id, extra);
            }
        }
    }
    Ok((p, unknown))
}

/// Heure actuelle en millisecondes depuis 1970 (voir `Project::saved_at`).
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Version de format d'un fichier JSON si elle est plus récente que celle-ci.
pub fn newer_format(json: &str) -> Option<u64> {
    #[derive(Deserialize)]
    struct Head {
        #[serde(default)]
        format_version: serde_json::Value,
    }
    let h: Head = serde_json::from_str(json).ok()?;
    h.format_version
        .as_u64()
        .filter(|&v| v > FORMAT_VERSION as u64)
}

/// Verrou d'écriture d'un fichier de session (avec le numéro de la dernière sauvegarde
/// écrite). Un verrou par fichier : la sauvegarde du double de la session sur un second
/// disque lent ou débranché ne bloque jamais celle de la session elle-même.
fn file_lock(path: &Path) -> std::sync::Arc<std::sync::Mutex<u64>> {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    static LOCKS: Mutex<Option<HashMap<PathBuf, Arc<Mutex<u64>>>>> = Mutex::new(None);
    let mut m = LOCKS.lock().unwrap_or_else(|e| e.into_inner());
    m.get_or_insert_with(HashMap::new)
        .entry(path.to_path_buf())
        .or_default()
        .clone()
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
        let snap_next = p.next_id;
        let t2 = p.add_track(Some("Nouvelle".into()), Some(1));
        let merged = merge_clips(p.tracks.clone(), snap, snap_next);
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

    /// Sauvegarde préparée pour la fin de session : écrite si rien de plus récent ne l'a
    /// été, abandonnée sinon (jamais d'ancien état par-dessus).
    #[test]
    fn deferred_save_never_overwrites_a_newer_save() {
        let dir = std::env::temp_dir().join(format!("tp-differe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut p = Project::new("D".into(), dir.clone(), 48000);
        p.add_track(Some("Ancienne".into()), None);
        let old = p.deferred_save();
        p.tracks[0].name = "Nouvelle".into();
        p.save().unwrap();
        old().unwrap();
        let file = p.file_path();
        let read = || Project::load(&file).unwrap().tracks[0].name.clone();
        assert_eq!(read(), "Nouvelle", "ancien état abandonné");
        p.tracks[0].name = "Dernière".into();
        p.dirty = true;
        let later = p.deferred_save();
        assert!(p.dirty, "la session reste à sauvegarder");
        later().unwrap();
        assert_eq!(read(), "Dernière");
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
        old.write_file(old_seq, false).unwrap();
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

    fn fresh_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tp-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// Fichier de session abîmé : la version précédente (`.bak1`) est reprise, le fichier
    /// abîmé est mis de côté (jamais écrasé), et la sauvegarde suivante le remplace.
    #[test]
    fn corrupt_session_falls_back_to_previous_version() {
        let dir = fresh_dir("bak");
        let mut p = Project::new("Concert".into(), dir.clone(), 48000);
        p.add_track(Some("Voix".into()), Some(0));
        p.save().unwrap();
        // Deuxième version : la première rejoint les copies (première copie : immédiate).
        p.tracks[0].name = "Voix lead".into();
        p.save().unwrap();
        let path = p.file_path();
        let baks = backup_paths(&path);
        assert!(baks[0].is_file(), "copie de la version précédente");
        // Rotation : au plus 3 copies, la plus récente en .bak1.
        for i in 0..5 {
            p.tracks[0].name = format!("V{i}");
            p.save_checkpoint().unwrap();
        }
        assert!(baks.iter().all(|b| b.is_file()));
        assert!(!path.with_file_name("Concert.tracksplus.bak4").exists());
        let b1 = Project::load(&baks[0]).unwrap();
        assert_eq!(b1.tracks[0].name, "V3");

        // Fichier abîmé : la lecture simple échoue, l'ouverture reprend .bak1.
        std::fs::write(&path, b"{\"tracks\": [ tronqu").unwrap();
        assert!(Project::load(&path).is_err());
        let (q, msg) = Project::open(&path, &[]).unwrap();
        assert!(msg.unwrap().contains("illisible"));
        assert_eq!(q.tracks[0].name, "V3");
        assert!(!q.dirty && q.read_only.is_none());
        let aside: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("Concert.tracksplus.illisible-"))
            .collect();
        assert_eq!(aside.len(), 1, "fichier abîmé conservé");
        assert_eq!(
            std::fs::read(dir.join(&aside[0])).unwrap(),
            b"{\"tracks\": [ tronqu"
        );
        // Toujours un fichier de session à sa place : la version reprise, déjà réécrite.
        assert_eq!(Project::load(&path).unwrap().tracks[0].name, "V3");
        let mut q = q;
        q.dirty = true;
        q.save().unwrap();
        assert_eq!(Project::load(&path).unwrap().tracks[0].name, "V3");

        // Plus aucune version lisible : erreur, rien n'est touché.
        std::fs::write(&path, b"abime").unwrap();
        for b in &baks {
            std::fs::write(b, b"abime aussi").unwrap();
        }
        let e = Project::open(&path, &[]).unwrap_err();
        assert!(
            format!("{e:#}").contains("aucune version précédente"),
            "{e:#}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"abime");
        // Repli fourni par l'appelant (double du second disque).
        let mut double = p.clone();
        double.dir = dir.join("double");
        double.tracks[0].name = "Double".into();
        double.save().unwrap();
        let (r, _) = Project::open(&path, &[double.file_path()]).unwrap();
        assert_eq!(
            (r.tracks[0].name.as_str(), r.dir.as_path()),
            ("Double", dir.as_path())
        );
        std::fs::remove_dir_all(dir).ok();
    }

    /// Fichier abîmé juste après une prise : la copie `.bak1` (état d'avant la prise) est
    /// plus ancienne que le double du second disque (état d'après) : c'est le double qui
    /// est repris. Plus tard, une copie `.bak1` plus récente que le double l'emporte.
    #[test]
    fn corrupt_session_takes_newest_backup() {
        let dir = fresh_dir("newest");
        let mut p = Project::new("N".into(), dir.clone(), 48000);
        p.add_track(Some("Avant la prise".into()), Some(0));
        p.save().unwrap();
        // Fin de prise : rotation (.bak1 = avant la prise), puis double à jour.
        std::thread::sleep(std::time::Duration::from_millis(5));
        p.tracks[0].name = "Après la prise".into();
        p.save_checkpoint().unwrap();
        let mut double = p.clone();
        double.dir = dir.join("double");
        double.mark_state_time();
        double.save().unwrap();
        let path = p.file_path();
        let bak1 = backup_paths(&path)[0].clone();
        assert_eq!(
            Project::load(&bak1).unwrap().tracks[0].name,
            "Avant la prise"
        );
        std::fs::write(&path, b"abime").unwrap();
        let (r, msg) = Project::open(&path, &[double.file_path()]).unwrap();
        assert_eq!(r.tracks[0].name, "Après la prise");
        assert!(msg.unwrap().contains("double"));

        // L'inverse : des sauvegardes plus récentes que le double (.bak1 plus récente).
        let mut p = r;
        std::thread::sleep(std::time::Duration::from_millis(5));
        p.tracks[0].name = "Plus tard".into();
        p.dirty = true;
        p.save_checkpoint().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        p.tracks[0].name = "Encore plus tard".into();
        p.save_checkpoint().unwrap();
        std::fs::write(&path, b"abime").unwrap();
        let (r, _) = Project::open(&path, &[double.file_path()]).unwrap();
        assert_eq!(r.tracks[0].name, "Plus tard");
        std::fs::remove_dir_all(dir).ok();
    }

    /// Version de secours d'un format plus récent : ouverte en lecture seule, et rien n'est
    /// touché sur le disque (le fichier abîmé reste en place, rien n'est mis de côté).
    #[test]
    fn read_only_backup_leaves_disk_untouched() {
        let dir = fresh_dir("robak");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("R.tracksplus");
        std::fs::write(&path, b"abime").unwrap();
        std::fs::write(
            &backup_paths(&path)[0],
            r#"{"format_version":99,"tracks":[]}"#,
        )
        .unwrap();
        let (p, msg) = Project::open(&path, &[]).unwrap();
        assert!(p.read_only.is_some() && msg.unwrap().contains("lecture seule"));
        assert_eq!(std::fs::read(&path).unwrap(), b"abime");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
        assert!(p.save_refused().is_some());
        std::fs::remove_dir_all(dir).ok();
    }

    /// Session qui ne peut pas être enregistrée (lecture seule, fichier d'une autre
    /// session) : signalé avant une prise, qui ne serait notée nulle part.
    #[test]
    fn save_refused_reports_permanent_refusals() {
        let dir = fresh_dir("refused");
        let mut p = Project::new("S".into(), dir.clone(), 48000);
        assert!(
            p.save_refused().is_none(),
            "session neuve, pas encore écrite"
        );
        p.save().unwrap();
        assert!(p.save_refused().is_none());
        let other = Project::new("S".into(), dir.clone(), 48000);
        assert!(other.save_refused().unwrap().contains("existe déjà"));
        let mut q = Project::load(&p.file_path()).unwrap();
        assert!(q.save_refused().is_none());
        q.read_only = Some("protégée".into());
        assert_eq!(q.save_refused().as_deref(), Some("protégée"));
        std::fs::remove_dir_all(dir).ok();
    }

    /// Champs inconnus d'une prise (gain, couleur… d'une version plus récente) : conservés
    /// à la réécriture, après une coupe et une renumérotation ; jamais à la place d'un champ
    /// connu.
    #[test]
    fn unknown_clip_fields_survive() {
        // La liste des champs connus suit la structure.
        let c = audio_clip(1, "a.wav");
        let v = serde_json::to_value(&c).unwrap();
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        let mut known = CLIP_FIELDS.to_vec();
        keys.sort_unstable();
        known.sort_unstable();
        assert_eq!(keys, known);

        let dir = fresh_dir("clipx");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("C.tracksplus");
        std::fs::write(
            &path,
            r#"{"format_version":1,"sample_rate":48000,"next_id":10,"tracks":[{"id":1,
            "name":"A","clips":[{"id":2,"file":"a.wav","start":0,"len":10000,"gain_db":-3.5,
            "couleur":{"r":1},"len_x":1},{"id":3,"file":"b.wav","start":20000,"len":500}]}]}"#,
        )
        .unwrap();
        let mut p = Project::load(&path).unwrap();
        assert!(p.read_only.is_none());
        let right = p.split_clip(1, 2, 4000).unwrap();
        p.dirty = true;
        p.save().unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let clips = v["tracks"][0]["clips"].as_array().unwrap();
        let by_id = |id: u64| clips.iter().find(|c| c["id"] == id).unwrap();
        assert_eq!(by_id(2)["gain_db"], -3.5);
        assert_eq!(by_id(2)["couleur"]["r"], 1);
        assert_eq!(by_id(2)["len"], 4000);
        assert_eq!(by_id(right)["gain_db"], -3.5);
        assert_eq!(by_id(right)["start"], 4000);
        assert!(by_id(3).get("gain_db").is_none());

        // Identifiants en double : renumérotés, les champs inconnus suivent leur prise.
        std::fs::write(
            &path,
            r#"{"tracks":[{"id":1,"clips":[{"id":1,"file":"a.wav","len":100,"x":"a"},
            {"id":7,"file":"b.wav","len":100,"x":"b"}]}]}"#,
        )
        .unwrap();
        let mut p = Project::load(&path).unwrap();
        p.dirty = true;
        p.save().unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let clips = &v["tracks"][0]["clips"];
        assert_eq!(clips[0]["file"], "a.wav");
        assert_eq!(clips[0]["x"], "a");
        assert_eq!(clips[1]["file"], "b.wav");
        assert_eq!(clips[1]["x"], "b");
        assert_ne!(clips[0]["id"], v["tracks"][0]["id"]);
        std::fs::remove_dir_all(dir).ok();
    }

    /// Fichier d'une version plus récente : numéro de format, champs et valeurs inconnus.
    /// Il s'ouvre, mais n'est jamais réécrit ; « Enregistrer sous » ailleurs reste possible.
    #[test]
    fn newer_format_is_read_only_and_unknown_fields_survive() {
        let dir = fresh_dir("newer");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Futur.tracksplus");
        // Champs inconnus seuls (ajout compatible, même format) : conservés à la réécriture.
        let json = r##"{"format_version":1,"sample_rate":48000,"bientot":{"x":1},
            "tracks":[{"id":1,"name":"A","clips":[],"couleur_rvb":"#ff0000"}],
            "markers":[{"id":2,"pos":5,"name":"M","note":"n"}]}"##;
        std::fs::write(&path, json).unwrap();
        let mut p = Project::load(&path).unwrap();
        assert!(p.read_only.is_none());
        p.dirty = true;
        p.save().unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["format_version"], FORMAT_VERSION);
        assert_eq!(v["bientot"]["x"], 1);
        assert_eq!(v["tracks"][0]["couleur_rvb"], "#ff0000");
        assert_eq!(v["markers"][0]["note"], "n");

        // Numéro de format plus récent : lecture seule.
        let newer = json.replace(r#""format_version":1"#, r#""format_version":99"#);
        std::fs::write(&path, &newer).unwrap();
        let mut p = Project::load(&path).unwrap();
        assert!(p.read_only.is_some());
        let e = p.save().unwrap_err();
        assert!(format!("{e:#}").contains("plus récente"), "{e:#}");
        assert!(p.save_in_background().recv().unwrap().is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            newer,
            "jamais réécrit"
        );
        // Copie ailleurs : modifiable.
        let mut q = p.clone();
        q.save_as(dir.join("Copie"), "Copie".into()).unwrap();
        assert!(q.read_only.is_none());
        assert!(Project::load(&q.file_path()).unwrap().read_only.is_none());

        // Variante inconnue (nouvelle sorte de piste) : session lisible, mais protégée.
        let variant = json.replace(r#""name":"A""#, r#""name":"A","kind":"Video""#);
        std::fs::write(&path, &variant).unwrap();
        let p = Project::load(&path).unwrap();
        assert_eq!(p.tracks[0].kind, TrackKind::Record);
        assert!(p.read_only.is_some());
        // Format plus récent qui ne se lit plus du tout : message clair, pas de repli.
        std::fs::write(&path, r#"{"format_version":7,"tracks":"autre chose"}"#).unwrap();
        let e = Project::open(&path, &[]).unwrap_err();
        assert!(format!("{e:#}").contains("plus récente"), "{e:#}");
        std::fs::remove_dir_all(dir).ok();
    }

    /// Session neuve : jamais écrite par-dessus un fichier de session existant (même nom,
    /// même dossier) ; le nom horodaté choisit un dossier libre.
    #[test]
    fn fresh_session_never_overwrites_existing_file() {
        let dir = fresh_dir("fresh");
        let mut old = Project::new("S".into(), dir.clone(), 48000);
        old.add_track(Some("Ancienne".into()), Some(0));
        old.save().unwrap();
        let mut neuf = Project::new("S".into(), dir.clone(), 48000);
        neuf.dirty = true;
        assert!(neuf.save().is_err());
        assert!(neuf.dirty);
        assert_eq!(
            Project::load(&old.file_path()).unwrap().tracks[0].name,
            "Ancienne"
        );
        // Une fois la session écrite, elle se réécrit normalement.
        old.tracks[0].name = "Modifiée".into();
        old.save().unwrap();
        assert_eq!(
            free_dir_name(dir.parent().unwrap(), "inexistant-tp"),
            "inexistant-tp"
        );
        let base = dir.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(
            free_dir_name(dir.parent().unwrap(), &base),
            format!("{base} (2)")
        );
        std::fs::remove_dir_all(dir).ok();
    }

    /// Une écriture précédente bloquée (disque figé) ne retient pas la suivante
    /// indéfiniment : refus après un délai borné, session toujours « à sauvegarder ».
    #[test]
    fn blocked_previous_write_does_not_block_forever() {
        let dir = fresh_dir("lock");
        let mut p = Project::new("L".into(), dir.clone(), 48000);
        p.save().unwrap();
        let lock = file_lock(&p.file_path());
        let held = lock.lock().unwrap();
        p.dirty = true;
        let t = std::time::Instant::now();
        let e = p.save().unwrap_err();
        assert!(t.elapsed() < std::time::Duration::from_secs(3));
        assert!(format!("{e:#}").contains("disque occupé"), "{e:#}");
        assert!(p.dirty);
        drop(held);
        p.save().unwrap();
        std::fs::remove_dir_all(dir).ok();
    }

    /// Session nommée « Soundboard » : son fichier temporaire et celui de `soundboard.json`
    /// ne se confondent jamais (Windows ignore la casse), même en écritures simultanées.
    #[test]
    fn concurrent_atomic_writes_use_distinct_temp_files() {
        let dir = fresh_dir("tmpnames");
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("Soundboard.tracksplus");
        let b = dir.join("soundboard.json");
        let ta = temp_sibling(&a, "tmp");
        let tb = temp_sibling(&b, "tmp");
        assert_ne!(
            ta.to_string_lossy().to_lowercase(),
            tb.to_string_lossy().to_lowercase()
        );
        assert_ne!(temp_sibling(&a, "tmp"), ta);
        let (a2, b2) = (a.clone(), b.clone());
        let h = std::thread::spawn(move || {
            for i in 0..50 {
                write_atomic(&a2, format!("{{\"session\":{i}}}").as_bytes()).unwrap();
            }
        });
        for i in 0..50 {
            write_atomic(&b2, format!("{{\"board\":{i}}}").as_bytes()).unwrap();
        }
        h.join().unwrap();
        assert!(std::fs::read_to_string(&a).unwrap().contains("session"));
        assert!(std::fs::read_to_string(&b).unwrap().contains("board"));
        let left = std::fs::read_dir(&dir).unwrap().flatten().count();
        assert_eq!(left, 2, "aucun fichier temporaire laissé");
        std::fs::remove_dir_all(dir).ok();
    }

    /// Renommage sans remplacement, et mise de côté sous un nom libre.
    #[test]
    fn rename_never_replaces_and_set_aside_keeps_everything() {
        let dir = fresh_dir("noreplace");
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.wav"), dir.join("b.wav"));
        std::fs::write(&a, b"A").unwrap();
        std::fs::write(&b, b"B").unwrap();
        let e = rename_no_replace(&a, &b).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&b).unwrap(), b"B");
        rename_no_replace(&a, &dir.join("c.wav")).unwrap();
        assert!(!a.exists());
        // Deux mises de côté dans la même seconde : deux noms différents.
        let f = dir.join("soundboard.json");
        std::fs::write(&f, b"1").unwrap();
        let s1 = set_aside(&f, "illisible", true).unwrap();
        std::fs::write(&f, b"2").unwrap();
        let s2 = set_aside(&f, "illisible", true).unwrap();
        assert_ne!(s1, s2);
        assert!(s1.to_string_lossy().ends_with(".json"));
        assert!(
            s1.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("soundboard.illisible-")
        );
        assert_eq!(std::fs::read(&s1).unwrap(), b"1");
        assert_eq!(std::fs::read(&s2).unwrap(), b"2");
        std::fs::remove_dir_all(dir).ok();
    }

    /// Import de plusieurs fichiers de même nom de base (« Intro.wav » et « Intro.mp3 »,
    /// ponctuation remplacée) : chaque fichier a son propre nom de destination.
    #[test]
    fn import_targets_never_share_a_file() {
        let dir = fresh_dir("import-names");
        let p = Project::new("I".into(), dir.clone(), 48000);
        p.ensure_dirs().unwrap();
        let mut taken = HashSet::new();
        let mut names = Vec::new();
        for stem in [
            "Intro", "Intro", "intro", "Take (1)", "Take [1]", "!!!", "???",
        ] {
            let f = p.new_take_file_avoiding(stem, &taken);
            taken.insert(f.to_lowercase());
            names.push(f.to_lowercase());
        }
        let unique: HashSet<&String> = names.iter().collect();
        assert_eq!(unique.len(), names.len(), "{names:?}");
        // Même si l'appelant oubliait les minuscules, le nom exact est reconnu.
        let mut raw = HashSet::new();
        let a = p.new_take_file_avoiding("Intro", &raw);
        raw.insert(a.clone());
        assert_ne!(p.new_take_file_avoiding("Intro", &raw), a);
        std::fs::remove_dir_all(dir).ok();
    }

    /// Annuler un import : la piste audio créée par l'import disparaît ; une piste
    /// d'enregistrement créée depuis reste, sans prise.
    #[test]
    fn undo_removes_imported_audio_track() {
        let mut p = Project::new("U".into(), std::env::temp_dir().join("tp-undo-imp"), 48000);
        p.add_track(Some("Voix".into()), Some(0));
        let snap = p.tracks.clone();
        let snap_next = p.next_id;
        let rec = p.add_track(Some("Basse".into()), Some(1));
        let audio = p.add_audio_track("Musique".into(), true);
        let id = p.alloc_id();
        p.track_mut(audio)
            .unwrap()
            .clips
            .push(audio_clip(id, "m.wav"));
        let merged = merge_clips(p.tracks.clone(), snap, snap_next);
        assert!(merged.iter().all(|t| t.id != audio));
        // Rétablir la suppression d'une piste : elle disparaît vraiment (pas une piste vide).
        let deleted_state: Vec<Track> = merged.iter().filter(|t| t.id != rec).cloned().collect();
        let redone = merge_clips(merged.clone(), deleted_state, p.next_id);
        assert!(redone.iter().all(|t| t.id != rec));
        let basse = merged.iter().find(|t| t.id == rec).unwrap();
        assert!(basse.clips.is_empty() && basse.input == Some(1));
    }

    /// Entrée d'une piste = numéro réel sur la carte : « IN 7 » marche en ASIO (carte
    /// entière) comme en WASAPI sur la paire « Analog (7+8) » (flux qui commence à 6), et
    /// pas sur la paire 1+2.
    #[test]
    fn track_input_is_the_real_card_channel() {
        let mut p = Project::new("E".into(), std::env::temp_dir(), 48000);
        let id = p.add_track_ch(Some("Micro".into()), Some(6), false);
        let t = p.tracks.iter().find(|t| t.id == id).unwrap();
        assert_eq!(t.input_label(), "IN 7");
        assert_eq!(t.engine_input(0), Some(6));
        assert!(t.input_fits(0, 18), "ASIO : carte entière");
        assert_eq!(t.engine_input(6), Some(0));
        assert!(t.input_fits(6, 2), "WASAPI : paire 7+8");
        assert!(!t.input_fits(0, 2), "WASAPI : paire 1+2");
        assert_eq!(t.engine_input(8), None);
    }

    /// Faders et panoramique : piste mono au centre à −3 dB de chaque côté, tout à gauche
    /// à 0 dB à gauche ; piste stéréo en balance ; fader en bas = silence.
    #[test]
    fn mix_gains_follow_fader_and_pan() {
        let mut p = Project::new("M".into(), std::env::temp_dir(), 48000);
        let mo = p.add_track(Some("Voix".into()), Some(0));
        let st = p.add_track_ch(Some("Synthé".into()), Some(2), true);
        let close =
            |a: [f32; 2], b: [f32; 2]| (a[0] - b[0]).abs() < 1e-5 && (a[1] - b[1]).abs() < 1e-5;
        let c = std::f32::consts::FRAC_1_SQRT_2;
        let t = |p: &Project, id| p.tracks.iter().find(|t| t.id == id).unwrap().mix_gains();
        assert!(close(t(&p, mo), [c, c]));
        assert!(close(t(&p, st), [1.0, 1.0]));
        p.track_mut(mo).unwrap().pan = -1.0;
        p.track_mut(st).unwrap().pan = 0.5;
        assert!(close(t(&p, mo), [1.0, 0.0]));
        assert!(close(t(&p, st), [0.5, 1.0]));
        p.track_mut(mo).unwrap().gain_db = FADER_MIN_DB;
        assert_eq!(t(&p, mo), [0.0, 0.0]);
        p.track_mut(st).unwrap().pan = f32::NAN;
        assert!(close(t(&p, st), [1.0, 1.0]), "valeur abîmée : au centre");
        assert!((db_to_gain(6.0) - 1.995).abs() < 1e-3);
        assert_eq!(db_to_gain(f32::NAN), 0.0);
        assert_eq!(db_to_gain(40.0), db_to_gain(FADER_MAX_DB));
    }
}
