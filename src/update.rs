//! Mises à jour : dernière version publiée sur GitHub (releases du dépôt), téléchargement
//! de l'installeur Windows et lancement de la mise à jour.
//!
//! Le réseau passe par `curl`, présent sur Windows 10/11, macOS et Linux : aucune
//! bibliothèque HTTP/TLS à embarquer. Tout se fait dans un thread : l'interface (et
//! l'audio) ne sont jamais bloqués.

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

/// Dépôt GitHub dont les releases sont les versions publiées.
pub const REPO: &str = "ElementV2/tracksplus";
/// Version de cette application.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// Version publiée.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    /// Notes de version (texte de la release, en Markdown).
    pub notes: String,
    /// Page de la release.
    pub page: String,
    /// Installeur Windows : nom, adresse, taille en octets.
    pub installer: Option<(String, String, u64)>,
    /// Empreinte SHA-256 de l'installeur (hexadécimal minuscule), si GitHub la fournit.
    pub sha256: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum State {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    /// Téléchargement de l'installeur (fichier `.part` en cours d'écriture, taille attendue).
    Downloading {
        release: Release,
        file: PathBuf,
        size: u64,
    },
    /// Installeur téléchargé, prêt à être lancé.
    Ready {
        release: Release,
        file: PathBuf,
    },
    Failed(String),
}

/// Réglages mémorisés (`%APPDATA%\TracksPlus\update.json`).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Settings {
    /// Recherche automatique au lancement.
    #[serde(default = "yes")]
    pub auto_check: bool,
    /// Version que l'utilisateur a choisi d'ignorer (plus proposée automatiquement).
    #[serde(default)]
    pub skipped: Option<String>,
}

fn yes() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            auto_check: true,
            skipped: None,
        }
    }
}

impl Settings {
    fn path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("TracksPlus").join("update.json"))
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

/// État partagé avec le thread de recherche / téléchargement.
#[derive(Clone, Default)]
pub struct Updater {
    state: Arc<Mutex<State>>,
}

impl Updater {
    pub fn state(&self) -> State {
        self.state.lock().clone()
    }

    pub fn set(&self, s: State) {
        *self.state.lock() = s;
    }

    pub fn busy(&self) -> bool {
        matches!(
            *self.state.lock(),
            State::Checking | State::Downloading { .. }
        )
    }

    /// Recherche la dernière version publiée (en arrière-plan).
    pub fn check(&self, ctx: &eframe::egui::Context) {
        if self.busy() {
            return;
        }
        self.set(State::Checking);
        let me = self.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let s = match latest_release() {
                Ok(r) if is_newer(&r.version, CURRENT) => State::Available(r),
                Ok(_) => State::UpToDate,
                Err(e) => State::Failed(e),
            };
            me.set(s);
            ctx.request_repaint();
        });
    }

    /// Télécharge l'installeur de `release` (en arrière-plan).
    pub fn download(&self, release: Release, ctx: &eframe::egui::Context) {
        let Some((name, url, size)) = release.installer.clone() else {
            return;
        };
        if self.busy() {
            return;
        }
        let dir = std::env::temp_dir().join("TracksPlus-update");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join(sanitize_file_name(&name));
        // Écrit d'abord dans `<nom>.part` : l'installeur n'existe sous son vrai nom qu'une
        // fois téléchargé en entier et vérifié.
        let part = part_path(&file);
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_file(&part);
        self.set(State::Downloading {
            release: release.clone(),
            file: part.clone(),
            size,
        });
        let me = self.clone();
        let ctx = ctx.clone();
        let digest = release.sha256.clone();
        std::thread::spawn(move || {
            let res = curl(&["-sSfL", "--max-time", "600", "-o"], Some(&part), &url)
                .and_then(|_| verify_download(&part, size, digest.as_deref()))
                .and_then(|_| {
                    std::fs::rename(&part, &file)
                        .map_err(|e| format!("impossible d'enregistrer l'installeur : {e}"))
                });
            me.set(match res {
                Ok(()) => State::Ready { release, file },
                Err(e) => {
                    let _ = std::fs::remove_file(&part);
                    State::Failed(e)
                }
            });
            ctx.request_repaint();
        });
    }
}

/// Fichier temporaire d'un téléchargement : `<fichier>.part`.
fn part_path(file: &Path) -> PathBuf {
    let mut n = file.file_name().unwrap_or_default().to_os_string();
    n.push(".part");
    file.with_file_name(n)
}

/// Vérifie un installeur téléchargé : empreinte SHA-256 si elle est connue, sinon taille.
fn verify_download(file: &Path, size: u64, sha256: Option<&str>) -> Result<(), String> {
    let got = std::fs::metadata(file).map(|m| m.len()).unwrap_or(0);
    if size > 0 && got != size {
        return Err(format!("téléchargement incomplet ({got} / {size} octets)"));
    }
    match sha256 {
        Some(want) => {
            let have = sha256_file(file).map_err(|e| format!("installeur illisible : {e}"))?;
            if have != want {
                return Err("installeur corrompu (empreinte SHA-256 différente)".into());
            }
            Ok(())
        }
        None if size == 0 => Err("taille inconnue : installeur impossible à vérifier".into()),
        None => Ok(()),
    }
}

/// Empreinte SHA-256 d'un fichier (hexadécimal minuscule), lu par blocs.
fn sha256_file(path: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Empreinte annoncée par GitHub (`"sha256:<hex>"`), en hexadécimal minuscule.
fn parse_digest(d: &str) -> Option<String> {
    let (algo, hex) = d.trim().split_once(':')?;
    let hex = hex.trim().to_ascii_lowercase();
    (algo.eq_ignore_ascii_case("sha256")
        && hex.len() == 64
        && hex.bytes().all(|b| b.is_ascii_hexdigit()))
    .then_some(hex)
}

/// Avancement d'un téléchargement (0 à 1), d'après la taille du fichier sur le disque.
pub fn progress(file: &Path, size: u64) -> f32 {
    let got = std::fs::metadata(file).map(|m| m.len()).unwrap_or(0);
    if size == 0 {
        0.0
    } else {
        (got as f64 / size as f64).clamp(0.0, 1.0) as f32
    }
}

/// Application installée avec l'installeur (sinon : version portable, ou autre système).
/// Seule une installation peut se mettre à jour toute seule.
pub fn installed() -> bool {
    cfg!(target_os = "windows")
        && std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|d| d.join("unins000.exe")))
            .is_some_and(|u| u.exists())
}

/// Mode d'installation à redonner à l'installeur : celui de l'installation en cours (dans
/// un dossier « Program Files » : pour tous les utilisateurs ; sinon : utilisateur courant).
/// Sans cela, l'installeur peut choisir l'autre mode (application lancée en administrateur,
/// par exemple) et créer une deuxième installation au lieu de mettre à jour la première.
fn install_mode(exe_dir: &Path, program_dirs: &[PathBuf]) -> &'static str {
    if program_dirs.iter().any(|d| exe_dir.starts_with(d)) {
        "/ALLUSERS"
    } else {
        "/CURRENTUSER"
    }
}

/// Lance l'installeur téléchargé en mode silencieux (fenêtre de progression seulement) ;
/// il ferme l'ancienne version, installe à la même place, puis relance TracksPlus.
/// L'application doit se fermer juste après.
pub fn launch_installer(file: &Path) -> Result<(), String> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf))
        .unwrap_or_default();
    let program_dirs: Vec<PathBuf> = ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"]
        .iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .collect();
    Command::new(file)
        .args([
            "/SILENT",
            "/SP-",
            "/NOCANCEL",
            "/NORESTART",
            "/CLOSEAPPLICATIONS",
            install_mode(&exe_dir, &program_dirs),
        ])
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("impossible de lancer l'installeur : {e}"))
}

/// Ouvre une adresse dans le navigateur.
pub fn open_url(url: &str) {
    #[cfg(target_os = "windows")]
    let _ = Command::new("explorer").arg(url).spawn();
    #[cfg(target_os = "macos")]
    let _ = Command::new("open").arg(url).spawn();
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    let _ = Command::new("xdg-open").arg(url).spawn();
}

/// Page de toutes les versions publiées.
pub fn releases_page() -> String {
    format!("https://github.com/{REPO}/releases")
}

// ---------------------------------------------------------------- réseau

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
    /// Empreinte du fichier, « sha256:<hex> » (absente des anciennes releases).
    #[serde(default)]
    digest: Option<String>,
}

fn latest_release() -> Result<Release, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let out = curl(
        &[
            "-sSfL",
            "--max-time",
            "15",
            "-H",
            "Accept: application/vnd.github+json",
        ],
        None,
        &url,
    )?;
    parse_release(&out)
}

fn parse_release(json: &str) -> Result<Release, String> {
    let r: GhRelease =
        serde_json::from_str(json).map_err(|_| "réponse de GitHub illisible".to_string())?;
    if r.draft || r.prerelease {
        return Err("aucune version publiée".into());
    }
    let asset = r.assets.iter().find(|a| {
        let n = a.name.to_lowercase();
        n.starts_with("tracksplus-setup") && n.ends_with(".exe")
    });
    let installer = asset.map(|a| (a.name.clone(), a.browser_download_url.clone(), a.size));
    let sha256 = asset
        .and_then(|a| a.digest.as_deref())
        .and_then(parse_digest);
    Ok(Release {
        version: r.tag_name.trim_start_matches(['v', 'V']).to_string(),
        notes: r.body.unwrap_or_default(),
        page: r.html_url,
        installer,
        sha256,
    })
}

/// Lance `curl` (sans fenêtre de console) : renvoie la sortie, ou écrit dans `out`.
fn curl(args: &[&str], out: Option<&Path>, url: &str) -> Result<String, String> {
    let mut cmd = Command::new("curl");
    cmd.args(args);
    if let Some(o) = out {
        cmd.arg(o);
    }
    cmd.args(["-A", &format!("TracksPlus/{CURRENT}")]).arg(url);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW : pas de console qui clignote.
        cmd.creation_flags(0x0800_0000);
    }
    let o = cmd
        .output()
        .map_err(|_| "curl introuvable sur cet ordinateur".to_string())?;
    if !o.status.success() {
        let err = String::from_utf8_lossy(&o.stderr);
        return Err(if err.contains("404") {
            "aucune version publiée trouvée".into()
        } else if err.contains("resolve") || err.contains("connect") || err.contains("timed out") {
            "pas de connexion à Internet".into()
        } else {
            let e = err.trim();
            if e.is_empty() {
                "échec de la connexion".into()
            } else {
                e.chars().take(140).collect()
            }
        });
    }
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

fn sanitize_file_name(name: &str) -> String {
    let s: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .collect();
    if s.is_empty() {
        "TracksPlus-Setup.exe".into()
    } else {
        s
    }
}

// ---------------------------------------------------------------- versions

fn parse_version(v: &str) -> Vec<u64> {
    v.trim()
        .trim_start_matches(['v', 'V'])
        .split(['.', '-', '+'])
        .take(3)
        .map(|p| p.parse().unwrap_or(0))
        .collect()
}

/// `a` est plus récente que `b` (versions « x.y.z »).
pub fn is_newer(a: &str, b: &str) -> bool {
    let (mut x, mut y) = (parse_version(a), parse_version(b));
    x.resize(3, 0);
    y.resize(3, 0);
    x > y
}

/// Notes de version (Markdown) en lignes simples : titres et puces gardés, mise en forme
/// retirée.
pub fn plain_notes(md: &str) -> Vec<(bool, String)> {
    md.lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let t = l.trim_start();
            let heading = t.starts_with('#');
            let t = t.trim_start_matches('#').trim();
            let t = if let Some(rest) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) {
                format!("•  {rest}")
            } else {
                t.to_string()
            };
            (heading, t.replace("**", "").replace('`', ""))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_keeps_install_mode() {
        let pf = vec![PathBuf::from(r"C:\Program Files")];
        assert_eq!(
            install_mode(Path::new(r"C:\Program Files\TracksPlus"), &pf),
            "/ALLUSERS"
        );
        assert_eq!(
            install_mode(
                Path::new(r"C:\Users\a\AppData\Local\Programs\TracksPlus"),
                &pf
            ),
            "/CURRENTUSER"
        );
    }

    #[test]
    fn versions_compare() {
        assert!(is_newer("0.3.0", "0.2.0"));
        assert!(is_newer("v1.0.0", "0.9.9"));
        assert!(is_newer("0.2.10", "0.2.9"));
        assert!(!is_newer("0.2.0", "0.2.0"));
        assert!(!is_newer("0.1.5", "0.2.0"));
        assert!(is_newer("0.3", "0.2.9"));
    }

    #[test]
    fn release_is_read_from_github_json() {
        let json = r###"{"tag_name":"v0.3.0","body":"## Nouveautés\n- **Pads** plus larges","html_url":"https://github.com/x/y/releases/tag/v0.3.0","draft":false,"prerelease":false,
            "assets":[{"name":"TracksPlus-0.3.0-windows-portable.zip","browser_download_url":"https://a/z.zip","size":5},
                      {"name":"TracksPlus-Setup-0.3.0.exe","browser_download_url":"https://a/s.exe","size":42}]}"###;
        let r = parse_release(json).unwrap();
        assert_eq!(r.version, "0.3.0");
        assert_eq!(
            r.installer,
            Some((
                "TracksPlus-Setup-0.3.0.exe".into(),
                "https://a/s.exe".into(),
                42
            ))
        );
        assert_eq!(r.sha256, None, "pas d'empreinte dans cette release");
        let notes = plain_notes(&r.notes);
        assert_eq!(notes[0], (true, "Nouveautés".into()));
        assert_eq!(notes[1], (false, "•  Pads plus larges".into()));
        assert!(parse_release("pas du json").is_err());
    }

    const ABC_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn digest_is_read_and_parsed() {
        let json = format!(
            r#"{{"tag_name":"v0.4.0","html_url":"https://x","assets":[
            {{"name":"TracksPlus-Setup-0.4.0.exe","browser_download_url":"https://a/s.exe","size":3,
              "digest":"sha256:{}"}}]}}"#,
            ABC_SHA256.to_uppercase()
        );
        let r = parse_release(&json).unwrap();
        assert_eq!(r.sha256.as_deref(), Some(ABC_SHA256));
        assert_eq!(
            parse_digest(&format!("sha256:{ABC_SHA256}")).as_deref(),
            Some(ABC_SHA256)
        );
        assert_eq!(parse_digest("sha512:abcd"), None);
        assert_eq!(parse_digest("sha256:xyz"), None);
        assert_eq!(parse_digest(ABC_SHA256), None);
    }

    #[test]
    fn download_is_verified() {
        let dir = std::env::temp_dir().join(format!("tp-upd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("TracksPlus-Setup.exe");
        assert_eq!(part_path(&f), dir.join("TracksPlus-Setup.exe.part"));
        std::fs::write(&f, b"abc").unwrap();
        assert_eq!(sha256_file(&f).unwrap(), ABC_SHA256);
        assert!(verify_download(&f, 3, Some(ABC_SHA256)).is_ok());
        assert!(verify_download(&f, 0, Some(ABC_SHA256)).is_ok());
        assert!(verify_download(&f, 3, None).is_ok());
        let bad = "0".repeat(64);
        assert!(
            verify_download(&f, 3, Some(&bad))
                .unwrap_err()
                .contains("SHA-256")
        );
        assert!(
            verify_download(&f, 4, None)
                .unwrap_err()
                .contains("incomplet")
        );
        assert!(
            verify_download(&f, 0, None)
                .unwrap_err()
                .contains("taille inconnue")
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn file_names_are_safe() {
        assert_eq!(
            sanitize_file_name("TracksPlus-Setup-0.3.0.exe"),
            "TracksPlus-Setup-0.3.0.exe"
        );
        assert_eq!(sanitize_file_name("../../x y.exe"), "....xy.exe");
    }
}
