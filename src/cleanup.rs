//! Nettoyage d'une session : fichiers audio que plus aucune prise n'utilise (pistes ou
//! prises supprimées, parties coupées), envoyés à la Corbeille sur demande. Rien n'est
//! jamais effacé tout seul : supprimer une piste ne touche pas à ses fichiers (Ctrl+Z).

use anyhow::{Result, bail};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Fichiers audio inutilisés d'une session et leur taille totale (octets).
#[derive(Default)]
pub struct Unused {
    pub files: Vec<PathBuf>,
    pub bytes: u64,
}

/// Nom de fichier comparé sans tenir compte de la casse (Windows).
pub fn key(name: &str) -> String {
    name.to_lowercase()
}

/// Fichiers audio posés directement dans `audio` dont le nom n'est pas dans `keep` (noms
/// passés par `key`). Les sous-dossiers (cache des formes d'onde…) et les fichiers qui ne
/// sont pas des sons ne sont jamais proposés. Dossier absent : rien.
pub fn unused_audio(audio: &Path, keep: &HashSet<String>) -> std::io::Result<Unused> {
    let mut out = Unused::default();
    let rd = match std::fs::read_dir(audio) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
    };
    for e in rd {
        let e = e?;
        if !e.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let path = e.path();
        if !crate::soundboard::is_audio_file(&path)
            || keep.contains(&key(&e.file_name().to_string_lossy()))
        {
            continue;
        }
        out.bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
        out.files.push(path);
    }
    out.files.sort();
    Ok(out)
}

/// Résultat d'une recherche : dossier `Audio` de la session et celui de son double sur le
/// second disque (copie de sécurité des prises).
#[derive(Default)]
pub struct Scan {
    pub main: Unused,
    pub backup: Unused,
    /// Second disque configuré mais pas nettoyé (absent, prise à récupérer, illisible).
    pub backup_skipped: Option<String>,
}

impl Scan {
    pub fn files(&self) -> impl Iterator<Item = &PathBuf> {
        self.main.files.iter().chain(&self.backup.files)
    }

    pub fn count(&self) -> usize {
        self.main.files.len() + self.backup.files.len()
    }

    pub fn bytes(&self) -> u64 {
        self.main.bytes + self.backup.bytes
    }
}

/// Fichiers utilisés par les sessions (`.tracksplus`) posées dans `dir` (noms passés par
/// `key`). Dossier absent : aucun.
fn session_refs(dir: &Path) -> std::result::Result<HashSet<String>, String> {
    let mut refs = HashSet::new();
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(refs),
        Err(e) => return Err(e.to_string()),
    };
    for e in rd.flatten() {
        let p = e.path();
        if !p
            .extension()
            .is_some_and(|x| x.eq_ignore_ascii_case(crate::project::PROJECT_EXT))
        {
            continue;
        }
        let text = std::fs::read_to_string(&p).map_err(|e| e.to_string())?;
        let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        for t in v["tracks"].as_array().into_iter().flatten() {
            for c in t["clips"].as_array().into_iter().flatten() {
                if let Some(name) = c["file"].as_str().and_then(|f| Path::new(f).file_name()) {
                    refs.insert(key(&name.to_string_lossy()));
                }
            }
        }
    }
    Ok(refs)
}

/// Noms (passés par `key`) de tous les fichiers posés dans `dir`.
fn names_in(dir: &Path) -> HashSet<String> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| key(&e.file_name().to_string_lossy()))
        .collect()
}

/// Inutilisés du double sur le second disque. Prudence : un fichier qui n'existe QUE sur le
/// second disque (trou d'entrée sur le seul disque principal : la copie a ses propres
/// fichiers) n'est retiré que si la session du double ne s'en sert pas non plus. Les copies
/// de fichiers de la session suivent la session.
fn backup_unused(
    bdir: &Path,
    main_audio: &Path,
    keep: &HashSet<String>,
) -> std::result::Result<Unused, String> {
    let refs = session_refs(bdir).map_err(|e| format!("2e disque : session illisible ({e})"))?;
    let main_names = names_in(main_audio);
    let all = unused_audio(&bdir.join(crate::project::AUDIO_DIR), keep)
        .map_err(|e| format!("2e disque illisible : {e}"))?;
    let mut out = Unused::default();
    for f in all.files {
        let name = key(&f.file_name().unwrap_or_default().to_string_lossy());
        if main_names.contains(&name) || !refs.contains(&name) {
            out.bytes += std::fs::metadata(&f).map(|m| m.len()).unwrap_or(0);
            out.files.push(f);
        }
    }
    Ok(out)
}

/// Recherche dans la session `dir` et dans son double `backup` (`(racine du second disque,
/// dossier du double)`). `keep` : fichiers des prises de la session ; `backup_keep` : en
/// plus, ceux que seule la copie possède (gardés sur le second disque). Un dossier qui garde
/// le journal d'une prise pas encore récupérée n'est jamais nettoyé : ses fichiers ne sont
/// peut-être pas encore dans la session.
pub fn scan(
    dir: &Path,
    backup: Option<(PathBuf, PathBuf)>,
    keep: &HashSet<String>,
    backup_keep: &HashSet<String>,
) -> std::result::Result<Scan, String> {
    if crate::recovery::journal_path(dir).exists() {
        return Err("Prise à récupérer en attente".into());
    }
    let main = unused_audio(&dir.join(crate::project::AUDIO_DIR), keep)
        .map_err(|e| format!("Dossier Audio illisible : {e}"))?;
    let mut out = Scan {
        main,
        ..Scan::default()
    };
    if let Some((root, bdir)) = backup {
        if !root.is_dir() {
            out.backup_skipped = Some("2e disque absent".into());
        } else if crate::recovery::journal_path(&bdir).exists() {
            out.backup_skipped = Some("2e disque : prise à récupérer".into());
        } else {
            let all: HashSet<String> = keep.union(backup_keep).cloned().collect();
            let main_audio = dir.join(crate::project::AUDIO_DIR);
            match backup_unused(&bdir, &main_audio, &all) {
                Ok(u) => out.backup = u,
                Err(e) => out.backup_skipped = Some(e),
            }
        }
    }
    Ok(out)
}

/// Après l'envoi à la Corbeille : caches de formes d'onde des fichiers retirés (recalculés
/// au besoin, donc effacés pour de bon).
pub fn drop_peak_caches(files: &[PathBuf]) {
    for f in files {
        if let (Some(dir), Some(name)) = (f.parent(), f.file_name()) {
            let mut cache = name.to_os_string();
            cache.push(".peaks");
            let _ = std::fs::remove_file(dir.join(".peaks").join(cache));
        }
    }
}

/// Envoie `files` à la Corbeille de Windows, d'un seul coup. Si Windows ne peut pas les y
/// mettre (disque réseau, clé USB), il demande confirmation avant de les effacer pour de
/// bon. Annulé par l'utilisateur : erreur, et rien n'est compté comme nettoyé.
#[cfg(windows)]
pub fn to_recycle_bin(files: &[PathBuf]) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::{
        FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT,
        FOF_WANTNUKEWARNING, SHFILEOPSTRUCTW, SHFileOperationW,
    };
    if files.is_empty() {
        return Ok(());
    }
    // Liste de chemins complets séparés par des zéros, terminée par un double zéro.
    let mut from: Vec<u16> = Vec::new();
    for f in files {
        let abs = std::path::absolute(f)?;
        from.extend(abs.as_os_str().encode_wide());
        from.push(0);
    }
    from.push(0);
    let mut op = SHFILEOPSTRUCTW {
        hwnd: std::ptr::null_mut(),
        wFunc: FO_DELETE,
        pFrom: from.as_ptr(),
        pTo: std::ptr::null(),
        fFlags: (FOF_ALLOWUNDO
            | FOF_NOCONFIRMATION
            | FOF_SILENT
            | FOF_NOERRORUI
            | FOF_WANTNUKEWARNING) as u16,
        fAnyOperationsAborted: 0,
        hNameMappings: std::ptr::null_mut(),
        lpszProgressTitle: std::ptr::null(),
    };
    // SAFETY : `from` (double zéro final) vit jusqu'à la fin de l'appel.
    let r = unsafe { SHFileOperationW(&mut op) };
    if op.fAnyOperationsAborted != 0 {
        bail!("annulé");
    }
    if r != 0 {
        bail!("Windows refuse (code {r:#x})");
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn to_recycle_bin(_files: &[PathBuf]) -> Result<()> {
    bail!("Corbeille indisponible sur ce système")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Seuls les sons du dossier qu'aucune prise n'utilise sont proposés : ni les fichiers
    /// utilisés (casse ignorée), ni le cache, ni ce qui n'est pas un son.
    #[test]
    fn lists_only_unused_sounds() {
        let dir = std::env::temp_dir().join(format!("tp-cleanup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".peaks")).unwrap();
        std::fs::write(dir.join("Voix_001.wav"), b"garde").unwrap();
        std::fs::write(dir.join("Voix_002.wav"), b"vieux").unwrap();
        std::fs::write(dir.join("notes.txt"), b"texte").unwrap();
        std::fs::write(dir.join(".peaks").join("Voix_002.wav.peaks"), b"c").unwrap();
        let keep: HashSet<String> = [key("VOIX_001.WAV")].into();
        let u = unused_audio(&dir, &keep).unwrap();
        assert_eq!(u.files, [dir.join("Voix_002.wav")]);
        assert_eq!(u.bytes, 5);
        let none = unused_audio(&dir.join("absent"), &keep).unwrap();
        assert!(none.files.is_empty());
        drop_peak_caches(&u.files);
        assert!(!dir.join(".peaks").join("Voix_002.wav.peaks").exists());
        std::fs::remove_dir_all(dir).ok();
    }

    /// Le double sur le second disque est nettoyé avec la session ; un journal de prise à
    /// récupérer bloque tout, et un second disque absent est seulement signalé.
    #[test]
    fn scan_covers_the_backup_copy() {
        let base = std::env::temp_dir().join(format!("tp-cleanup-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (dir, root) = (base.join("Session"), base.join("Copie"));
        let bdir = root.join("Session (1234)");
        for d in [&dir, &bdir] {
            std::fs::create_dir_all(d.join(crate::project::AUDIO_DIR)).unwrap();
            std::fs::write(d.join(crate::project::AUDIO_DIR).join("a.wav"), b"aa").unwrap();
            std::fs::write(d.join(crate::project::AUDIO_DIR).join("b.wav"), b"bbb").unwrap();
        }
        let baudio = bdir.join(crate::project::AUDIO_DIR);
        // Propres au second disque : « alt.wav » (utilisé par la session du double),
        // « mem.wav » (connu seulement de l'application), « orphelin.wav » (personne).
        for f in ["alt.wav", "mem.wav", "orphelin.wav"] {
            std::fs::write(baudio.join(f), b"z").unwrap();
        }
        // La session du double, pas encore à jour, cite encore « b.wav ».
        std::fs::write(
            bdir.join("Session.tracksplus"),
            r#"{"tracks":[{"clips":[{"file":"b.wav"},{"file":"ALT.wav"}]}]}"#,
        )
        .unwrap();
        let keep: HashSet<String> = [key("a.wav")].into();
        let bkeep: HashSet<String> = [key("mem.wav")].into();
        let s = scan(&dir, Some((root.clone(), bdir.clone())), &keep, &bkeep).unwrap();
        let names = |u: &Unused| {
            u.files
                .iter()
                .map(|f| f.file_name().unwrap().to_string_lossy().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&s.main), ["b.wav"]);
        assert_eq!(names(&s.backup), ["b.wav", "orphelin.wav"]);
        assert_eq!(s.bytes(), 7);
        assert!(s.backup_skipped.is_none());
        let s = scan(&dir, Some((base.join("Absent"), bdir.clone())), &keep, &bkeep).unwrap();
        assert_eq!((s.count(), s.backup_skipped.is_some()), (1, true));
        std::fs::write(bdir.join("Session.tracksplus"), "abîmé").unwrap();
        let s = scan(&dir, Some((root.clone(), bdir.clone())), &keep, &bkeep).unwrap();
        assert_eq!((s.count(), s.backup_skipped.is_some()), (1, true));
        std::fs::remove_file(bdir.join("Session.tracksplus")).unwrap();
        std::fs::write(crate::recovery::journal_path(&bdir), "{}").unwrap();
        let s = scan(&dir, Some((root, bdir)), &keep, &bkeep).unwrap();
        assert_eq!((s.count(), s.backup_skipped.is_some()), (1, true));
        std::fs::write(crate::recovery::journal_path(&dir), "{}").unwrap();
        assert!(scan(&dir, None, &keep, &bkeep).is_err());
        std::fs::remove_dir_all(base).ok();
    }
}
