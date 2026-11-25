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
    /// Textes calculés une fois à la lecture (pas à chaque image) : date lisible,
    /// résumé « 4 pistes · 12 prises · 12:34 », nom en minuscules pour la recherche.
    pub date: String,
    pub summary: String,
    pub lower: String,
}

/// Réglages de l'écran d'accueil (`%APPDATA%\TracksPlus\startup.json`).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct StartupPrefs {
    /// Sessions ouvertes récemment (la plus récente en premier).
    #[serde(default)]
    pub recent: Vec<PathBuf>,
    /// Dernier dossier où des sons ont été choisis (explorateur de sons).
    #[serde(default)]
    pub sound_dir: Option<PathBuf>,
    /// Ancien emplacement du dossier de la copie de sécurité (désormais `backup.json`, voir
    /// `load_backup_root`) : lu s'il n'y a pas encore de `backup.json`, et gardé pour les
    /// versions précédentes de l'application.
    #[serde(default)]
    pub backup_dir: Option<PathBuf>,
    /// Champs inconnus (version plus récente) : réécrits tels quels.
    #[serde(flatten)]
    pub extra: crate::project::Extra,
}

/// Erreur de lecture d'un fichier de réglages.
#[derive(Debug)]
struct PrefsError {
    msg: String,
    /// Le fichier se lit mais son contenu n'est pas valide (abîmé pour de bon), par
    /// opposition à une erreur de lecture, peut-être passagère (fichier verrouillé).
    invalid: bool,
}

/// Lit un fichier de réglages JSON : `Ok(None)` s'il n'existe pas encore, `Err` s'il ne se
/// lit pas (verrouillé, abîmé) — jamais des réglages par défaut qui l'écraseraient ensuite.
/// Trois essais espacés de 50 ms.
fn read_prefs<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>, PrefsError> {
    let mut last = PrefsError {
        msg: String::new(),
        invalid: false,
    };
    for i in 0..3 {
        if i > 0 {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        match std::fs::read_to_string(path) {
            Ok(s) => match serde_json::from_str(&s) {
                Ok(v) => return Ok(Some(v)),
                Err(e) => {
                    last = PrefsError {
                        msg: format!("{} illisible ({e})", path.display()),
                        invalid: true,
                    }
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                last = PrefsError {
                    msg: format!("lecture de {} impossible ({e})", path.display()),
                    invalid: false,
                }
            }
        }
    }
    Err(last)
}

/// Message à montrer une fois à l'utilisateur : réglages de l'accueil abîmés, remis à zéro
/// (voir `StartupPrefs::update`). Relevé par l'interface (`take_prefs_notice`).
static PREFS_NOTICE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Message en attente sur les réglages de l'accueil (une seule fois).
pub fn take_prefs_notice() -> Option<String> {
    PREFS_NOTICE.try_lock().ok().and_then(|mut n| n.take())
}

impl StartupPrefs {
    fn path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("TracksPlus").join("startup.json"))
    }

    /// Lecture pour affichage : réglages par défaut si le fichier ne se lit pas. Pour les
    /// modifier, passer par `update` (qui n'écrase jamais un fichier illisible).
    pub fn load() -> Self {
        Self::path()
            .and_then(|p| read_prefs(&p).ok().flatten())
            .unwrap_or_default()
    }

    fn save_to(&self, path: &Path) -> bool {
        match serde_json::to_string_pretty(self) {
            Ok(s) => crate::project::write_atomic(path, s.as_bytes()).is_ok(),
            Err(_) => false,
        }
    }

    /// Modifie les réglages enregistrés. Fichier momentanément illisible (verrouillé) :
    /// rien n'est écrit. Fichier abîmé pour de bon : mis de côté (`startup.illisible-….json`),
    /// les réglages repartent de zéro, et l'utilisateur en est prévenu une fois (au lieu de
    /// ne plus jamais pouvoir les mettre à jour). Renvoie `true` si c'est écrit.
    pub fn update(f: impl FnOnce(&mut Self)) -> bool {
        match Self::path() {
            Some(path) => Self::update_at(&path, f),
            None => false,
        }
    }

    fn update_at(path: &Path, f: impl FnOnce(&mut Self)) -> bool {
        let mut prefs = match read_prefs::<Self>(path) {
            Ok(prefs) => prefs.unwrap_or_default(),
            Err(e) if e.invalid => match set_aside_invalid(path, &e) {
                Some(()) => Self::default(),
                None => return false,
            },
            Err(e) => {
                eprintln!("Réglages de l'accueil non modifiés : {}", e.msg);
                return false;
            }
        };
        f(&mut prefs);
        prefs.save_to(path)
    }
}

/// Réglages de l'accueil abîmés pour de bon : le fichier est mis de côté et l'utilisateur
/// prévenu (`PREFS_NOTICE`). `None` si la mise de côté a échoué (rien n'est alors écrit).
fn set_aside_invalid(path: &Path, e: &PrefsError) -> Option<()> {
    match crate::project::set_aside(path, "illisible", true) {
        Ok(aside) => {
            let mut msg = format!(
                "Réglages de l'accueil illisibles ({}) : sessions récentes et dossier des sons                  remis à zéro. L'ancien fichier est conservé sous « {} ».",
                e.msg,
                aside.file_name().unwrap_or_default().to_string_lossy()
            );
            // Ancien emplacement du réglage de la copie de sécurité : perdu avec ce fichier.
            if backup_prefs_path().is_some_and(|b| !b.is_file()) {
                msg += " Si une copie de sécurité était réglée, choisissez de nouveau le second disque.";
            }
            if let Ok(mut n) = PREFS_NOTICE.lock() {
                *n = Some(msg);
            }
            Some(())
        }
        Err(err) => {
            eprintln!(
                "Réglages de l'accueil non modifiés : {} (mise de côté impossible : {err})",
                e.msg
            );
            None
        }
    }
}

/// Au lancement : réglages de l'accueil abîmés pour de bon mis de côté tout de suite (la
/// liste de l'accueil repart de zéro, l'utilisateur est prévenu). Renvoie le message.
pub fn check_startup_prefs() -> Option<String> {
    let path = StartupPrefs::path()?;
    if let Err(e) = read_prefs::<StartupPrefs>(&path)
        && e.invalid
    {
        set_aside_invalid(&path, &e);
    }
    take_prefs_notice()
}

/// Dossier de la copie de sécurité sur un second disque (`%APPDATA%\TracksPlus\backup.json`),
/// séparé des réglages de l'accueil réécrits à chaque prise : il n'est écrit que quand
/// l'utilisateur le change.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct BackupPrefs {
    #[serde(default)]
    backup_dir: Option<PathBuf>,
    #[serde(flatten)]
    extra: crate::project::Extra,
}

fn backup_prefs_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("TracksPlus").join("backup.json"))
}

/// Dossier de la copie de sécurité. `Err` : le réglage existe mais ne se lit pas (la copie
/// est peut-être désactivée à tort, à signaler).
pub fn load_backup_root() -> Result<Option<PathBuf>, String> {
    let Some(path) = backup_prefs_path() else {
        return Ok(None);
    };
    match read_prefs::<BackupPrefs>(&path).map_err(|e| e.msg)? {
        Some(b) => Ok(b.backup_dir),
        // Pas encore de `backup.json` : ancien emplacement.
        None => match StartupPrefs::path() {
            Some(p) => Ok(read_prefs::<StartupPrefs>(&p)
                .map_err(|e| e.msg)?
                .and_then(|s| s.backup_dir)),
            None => Ok(None),
        },
    }
}

/// Enregistre le dossier de la copie de sécurité (et à l'ancien emplacement, pour les
/// versions précédentes de l'application).
pub fn save_backup_root(dir: Option<&Path>) -> anyhow::Result<()> {
    let path =
        backup_prefs_path().ok_or_else(|| anyhow::anyhow!("dossier des réglages introuvable"))?;
    let mut b = read_prefs::<BackupPrefs>(&path)
        .ok()
        .flatten()
        .unwrap_or_default();
    b.backup_dir = dir.map(Path::to_path_buf);
    crate::project::write_atomic(&path, serde_json::to_string_pretty(&b)?.as_bytes())?;
    StartupPrefs::update(|p| p.backup_dir = dir.map(Path::to_path_buf));
    Ok(())
}

/// Mémorise une session ouverte ou enregistrée (en tête des récentes).
pub fn remember(path: &Path) {
    StartupPrefs::update(|prefs| {
        prefs.recent.retain(|p| p != path);
        prefs.recent.insert(0, path.to_path_buf());
        prefs.recent.truncate(RECENT_MAX);
    });
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

/// Résumé d'une session (lit son fichier) ; `None` si elle est absente. Une session
/// illisible reste dans la liste, signalée : l'ouvrir reprend sa version précédente
/// (voir `Project::open`), ses prises sont toujours dans son dossier Audio.
pub fn info(path: &Path) -> Option<SessionInfo> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let p = match Project::load(path) {
        Ok(p) => p,
        Err(e) => {
            let name = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let text = std::fs::read_to_string(path).unwrap_or_default();
            let summary = if crate::project::newer_format(&text).is_some() {
                "Version plus récente de TracksPlus requise".to_string()
            } else if crate::project::backup_paths(path)
                .iter()
                .any(|b| b.is_file())
            {
                "Fichier illisible · ouvrir pour reprendre la version précédente".to_string()
            } else {
                format!("Fichier illisible ({})", short_error(&e))
            };
            return Some(SessionInfo {
                path: path.to_path_buf(),
                lower: name.to_lowercase(),
                name,
                modified,
                tracks: 0,
                takes: 0,
                date: fmt_date(modified),
                summary,
            });
        }
    };
    let sr = p.sample_rate.max(1) as f64;
    let tracks = p.tracks.len();
    let takes = p.tracks.iter().map(|t| t.clips.len()).sum();
    let duration = p.end() as f64 / sr;
    let mut summary = fmt_summary(tracks, takes, duration);
    if p.read_only.is_some() {
        summary += " · version plus récente (lecture seule)";
    }
    Some(SessionInfo {
        path: path.to_path_buf(),
        lower: p.name.to_lowercase(),
        name: p.name,
        modified,
        tracks,
        takes,
        date: fmt_date(modified),
        summary,
    })
}

/// Cause d'une erreur, courte (première ligne de la cause la plus profonde).
fn short_error(e: &anyhow::Error) -> String {
    let s = e.root_cause().to_string();
    let first = s.lines().next().unwrap_or_default();
    first.chars().take(60).collect()
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
        // Session illisible : toujours dans la liste, signalée (ses prises existent).
        a.tracks[0].name = "B".into();
        a.save().unwrap();
        a.save_checkpoint().unwrap();
        std::fs::write(a.file_path(), b"abime").unwrap();
        let bad = info(&a.file_path()).unwrap();
        assert_eq!(bad.name, "A");
        assert!(
            bad.summary.contains("version précédente"),
            "{}",
            bad.summary
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// Réglages : un fichier absent donne `None`, un fichier illisible une erreur (jamais
    /// des réglages par défaut, qui l'écraseraient ensuite, copie de sécurité comprise).
    #[test]
    fn unreadable_prefs_are_an_error_not_defaults() {
        let dir = std::env::temp_dir().join(format!("tp-prefs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("startup.json");
        assert!(matches!(read_prefs::<StartupPrefs>(&p), Ok(None)));
        std::fs::write(&p, br#"{"backup_dir":"E:\\Copie","recent":[],"futur":1}"#).unwrap();
        let s = read_prefs::<StartupPrefs>(&p).unwrap().unwrap();
        assert_eq!(s.backup_dir, Some(PathBuf::from("E:\\Copie")));
        assert_eq!(serde_json::to_value(&s).unwrap()["futur"], 1);
        std::fs::write(&p, b"{\"backup_dir\": tronqu").unwrap();
        assert!(read_prefs::<StartupPrefs>(&p).is_err_and(|e| e.invalid));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Réglages de l'accueil abîmés pour de bon : mis de côté (jamais perdus), la mise à
    /// jour repart de zéro au lieu d'être refusée pour toujours, et l'utilisateur est
    /// prévenu une fois.
    #[test]
    fn invalid_startup_prefs_are_set_aside_and_reset() {
        let dir = std::env::temp_dir().join(format!("tp-prefs-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("startup.json");
        // Fichier valide : modifié, champs inconnus gardés.
        std::fs::write(&p, br#"{"recent":["A"],"futur":1}"#).unwrap();
        assert!(StartupPrefs::update_at(&p, |s| s.recent.push("B".into())));
        let s = read_prefs::<StartupPrefs>(&p).unwrap().unwrap();
        assert_eq!(s.recent, [PathBuf::from("A"), PathBuf::from("B")]);
        assert_eq!(s.extra["futur"], 1);
        // Fichier abîmé : mis de côté, réglages repartis de zéro.
        std::fs::write(&p, b"{\"recent\": [tronqu").unwrap();
        assert!(StartupPrefs::update_at(&p, |s| s.recent.push("C".into())));
        let s = read_prefs::<StartupPrefs>(&p).unwrap().unwrap();
        assert_eq!(s.recent, [PathBuf::from("C")]);
        let aside: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("startup.illisible-") && n.ends_with(".json"))
            .collect();
        assert_eq!(aside.len(), 1);
        assert_eq!(
            std::fs::read(dir.join(&aside[0])).unwrap(),
            b"{\"recent\": [tronqu"
        );
        let notice = take_prefs_notice().unwrap();
        assert!(notice.contains(&aside[0]), "{notice}");
        assert!(take_prefs_notice().is_none(), "prévenu une seule fois");
        let _ = std::fs::remove_dir_all(dir);
    }
}
