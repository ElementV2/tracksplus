//! Sessions connues, pour l'écran d'accueil : celles du dossier des sessions
//! (`Musique\TracksPlus`) et les sessions récentes ouvertes ailleurs.

use crate::project::{PROJECT_EXT, Project, default_sessions_root};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Nombre de sessions récentes mémorisées.
const RECENT_MAX: usize = 30;

/// Résumé d'une session, affiché dans la liste.
#[derive(Clone, Debug)]
pub struct SessionInfo {
    pub path: PathBuf,
    pub name: String,
    pub modified: SystemTime,
    #[cfg_attr(not(test), allow(dead_code))]
    pub tracks: usize,
    pub takes: usize,
    /// Durée (secondes) jusqu'à la fin de la dernière prise.
    #[allow(dead_code)]
    pub duration: f64,
    /// Textes calculés une fois à la lecture (pas à chaque image) : date lisible,
    /// résumé « 4 pistes · 12 prises · 12:34 », nom en minuscules pour la recherche.
    pub date: String,
    pub summary: String,
    pub lower: String,
}

/// Réglages de l'écran d'accueil (`%APPDATA%\TracksPlus\startup.json`).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct StartupPrefs {
    #[serde(default = "yes")]
    pub show_welcome: bool,
    /// Sessions ouvertes récemment (la plus récente en premier).
    #[serde(default)]
    pub recent: Vec<PathBuf>,
    /// Dernier dossier où des sons ont été choisis (explorateur de sons).
    #[serde(default)]
    pub sound_dir: Option<PathBuf>,
}

fn yes() -> bool {
    true
}

impl Default for StartupPrefs {
    fn default() -> Self {
        Self {
            show_welcome: true,
            recent: Vec::new(),
            sound_dir: None,
        }
    }
}

impl StartupPrefs {
    fn path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("TracksPlus").join("startup.json"))
    }

    pub fn load() -> Self {
        Self::path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        if let (Some(p), Ok(s)) = (Self::path(), serde_json::to_string_pretty(self)) {
            let _ = crate::project::write_atomic(&p, s.as_bytes());
        }
    }
}

/// Mémorise une session ouverte ou enregistrée (en tête des récentes).
pub fn remember(path: &Path) {
    let mut prefs = StartupPrefs::load();
    prefs.recent.retain(|p| p != path);
    prefs.recent.insert(0, path.to_path_buf());
    prefs.recent.truncate(RECENT_MAX);
    prefs.save();
}

/// Sessions connues, de la plus récemment modifiée à la plus ancienne.
pub fn list() -> Vec<SessionInfo> {
    let mut paths: Vec<PathBuf> = StartupPrefs::load().recent;
    // Dossier des sessions : <racine>/<session>/<session>.tracksplus
    if let Ok(dirs) = std::fs::read_dir(default_sessions_root()) {
        for d in dirs.flatten() {
            let Ok(files) = std::fs::read_dir(d.path()) else {
                continue;
            };
            for f in files.flatten() {
                let p = f.path();
                if p.extension().is_some_and(|e| e == PROJECT_EXT) {
                    paths.push(p);
                }
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<SessionInfo> = paths
        .into_iter()
        .filter(|p| seen.insert(p.canonicalize().unwrap_or_else(|_| p.clone())))
        .filter_map(|p| info(&p))
        .collect();
    out.sort_by_key(|a| std::cmp::Reverse(a.modified));
    out
}

/// Résumé d'une session (lit son fichier) ; `None` si elle est absente ou illisible.
pub fn info(path: &Path) -> Option<SessionInfo> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let p = Project::load(path).ok()?;
    let sr = p.sample_rate.max(1) as f64;
    let tracks = p.tracks.len();
    let takes = p.tracks.iter().map(|t| t.clips.len()).sum();
    let duration = p.end() as f64 / sr;
    Some(SessionInfo {
        path: path.to_path_buf(),
        lower: p.name.to_lowercase(),
        name: p.name,
        modified,
        tracks,
        takes,
        duration,
        date: fmt_date(modified),
        summary: fmt_summary(tracks, takes, duration),
    })
}

/// « 4 pistes · 12 prises · 12:34 », ou « 2 pistes · vide ».
pub fn fmt_summary(tracks: usize, takes: usize, duration: f64) -> String {
    let pistes = if tracks > 1 { "pistes" } else { "piste" };
    if takes == 0 {
        format!("{tracks} {pistes} · vide")
    } else {
        let prises = if takes > 1 { "prises" } else { "prise" };
        format!(
            "{tracks} {pistes} · {takes} {prises} · {}",
            fmt_length(duration)
        )
    }
}

/// Date lisible : « Aujourd'hui 21:34 », « Hier 18:02 », sinon « 24/11/2026 ».
pub fn fmt_date(t: SystemTime) -> String {
    let dt: chrono::DateTime<chrono::Local> = t.into();
    let today = chrono::Local::now().date_naive();
    let d = dt.date_naive();
    if d == today {
        format!("Aujourd'hui {}", dt.format("%H:%M"))
    } else if today.pred_opt() == Some(d) {
        format!("Hier {}", dt.format("%H:%M"))
    } else {
        dt.format("%d/%m/%Y").to_string()
    }
}

/// Durée « 1 h 02 », « 12:34 » ou « 0:05 ».
pub fn fmt_length(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    if s >= 3600 {
        format!("{} h {:02}", s / 3600, s / 60 % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Charge : 300 sessions (8 pistes, 50 prises chacune) lues pour l'écran d'accueil.
    /// `cargo test --release -- --ignored --nocapture charge_accueil`
    #[test]
    #[ignore]
    fn charge_accueil_300_sessions() {
        use crate::project::Clip;
        let root = std::env::temp_dir().join(format!("tp-charge-acc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut paths = Vec::new();
        for s in 0..300 {
            let mut p = Project::new(format!("S{s:03}"), root.join(format!("S{s:03}")), 48000);
            for t in 0..8u16 {
                let id = p.add_track(None, Some(t));
                for k in 0..50u64 {
                    p.track_mut(id).unwrap().clips.push(Clip {
                        id: 10 + k,
                        file: format!("{t}_{k}.wav"),
                        start: k * 480_000,
                        offset: 0,
                        len: 400_000,
                        src_len: 400_000,
                        fade_in: 0,
                        fade_out: 0,
                    });
                }
            }
            p.save().unwrap();
            paths.push(p.file_path());
        }
        let t = std::time::Instant::now();
        let infos: Vec<SessionInfo> = paths.iter().filter_map(|p| info(p)).collect();
        let d = t.elapsed();
        println!("accueil : {} sessions lues en {d:?}", infos.len());
        assert_eq!(infos.len(), 300);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn lengths_and_dates() {
        assert_eq!(fmt_length(5.0), "0:05");
        assert_eq!(fmt_length(754.4), "12:34");
        assert_eq!(fmt_length(3725.0), "1 h 02");
        assert!(fmt_date(SystemTime::now()).starts_with("Aujourd'hui"));
        let two_days = SystemTime::now() - std::time::Duration::from_secs(3 * 86_400);
        assert!(fmt_date(two_days).contains('/'));
    }

    #[test]
    fn sessions_are_read_and_sorted() {
        let root = std::env::temp_dir().join(format!("tp-sessions-{}", std::process::id()));
        let mut a = Project::new("A".into(), root.join("A"), 48000);
        a.add_track(None, Some(0));
        a.save().unwrap();
        let ia = info(&a.file_path()).unwrap();
        assert_eq!((ia.name.as_str(), ia.tracks, ia.takes), ("A", 1, 0));
        assert_eq!(ia.summary, "1 piste · vide");
        assert!(ia.date.starts_with("Aujourd'hui"));
        assert_eq!(ia.lower, "a");
        assert_eq!(fmt_summary(4, 12, 754.0), "4 pistes · 12 prises · 12:34");
        assert!(info(&root.join("absent.tracksplus")).is_none());
        let _ = std::fs::remove_dir_all(root);
    }
}
