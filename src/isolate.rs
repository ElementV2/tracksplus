//! Décodage isolé : les fichiers reçus d'ailleurs (pads, import sur la timeline) sont
//! décodés dans un processus fils — le même exécutable, lancé avec `WORKER_ARG`.
//!
//! Un décodeur piégé ou un fichier abîmé que les contrôles de `decode` n'auraient pas
//! repéré (table de codec démesurée, boucle sans fin…) n'arrête alors que ce processus :
//! la carte son, la prise en cours et le soundboard continuent. Sous Windows, le fils est
//! placé dans un « Job Object » qui borne sa mémoire (une réservation géante échoue au lieu
//! de faire gonfler le fichier d'échange en plein spectacle), le tue si TracksPlus se ferme,
//! et n'affiche aucune fenêtre d'erreur. Il tourne en priorité basse.
//!
//! Échanges : une demande sur l'entrée du fils, des messages sur sa sortie (avancement,
//! segments écrits, son décodé, erreur). Sans nouvelle pendant `SILENCE_TIMEOUT`, le fils
//! est arrêté. Si le fils ne peut pas être lancé, le décodage se fait dans le processus
//! (avec les contrôles de `decode`).

use anyhow::{Result, anyhow, bail};
use parking_lot::Mutex;
use std::collections::HashSet;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Argument qui lance l'exécutable en processus de décodage.
pub const WORKER_ARG: &str = "--tracksplus-decode-worker";
const MAGIC: [u8; 4] = *b"TPD1";
const KIND_PAD: u8 = 1;
const KIND_IMPORT: u8 = 2;

const MSG_PROGRESS: u8 = b'P';
/// Taille attendue d'un son de pad (trames), dès l'ouverture du fichier.
const MSG_SIZE: u8 = b'Z';
/// Nom définitif d'un segment d'import, juste avant que le fichier n'y soit renommé.
const MSG_NAMING: u8 = b'N';
const MSG_SEGMENT: u8 = b'S';
const MSG_DATA: u8 = b'D';
const MSG_DONE: u8 = b'O';
const MSG_ERROR: u8 = b'E';

/// Variable d'environnement posée pour le processus fils.
const CHILD_ENV: &str = "TRACKSPLUS_DECODE_CHILD";

/// Sans aucun message du fils pendant ce temps, il est arrêté (décodeur bloqué).
const SILENCE_TIMEOUT: Duration = Duration::from_secs(60);
/// Intervalle des messages d'avancement du fils (il prouve qu'il travaille).
const HEARTBEAT: Duration = Duration::from_millis(250);

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Active le décodage isolé (appelé par `main` : jamais dans les tests, dont l'exécutable
/// ne sait pas jouer le rôle du processus fils).
pub fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}

fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Point d'entrée du processus fils : `Some(code de sortie)` si l'exécutable a été lancé
/// pour décoder, `None` sinon (lancement normal de l'application).
pub fn worker_main() -> Option<i32> {
    if std::env::args_os().nth(1).as_deref() != Some(std::ffi::OsStr::new(WORKER_ARG)) {
        return None;
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Diagnostics::Debug::{
            SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX, SEM_NOOPENFILEERRORBOX, SetErrorMode,
        };
        // SAFETY : simple réglage du processus, sans pointeur.
        unsafe {
            SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX);
        }
    }
    // Une panique du décodeur devient un message d'erreur (voir `serve`), sans trace.
    std::panic::set_hook(Box::new(|_| {}));
    let mut input = std::io::stdin().lock();
    let out = std::cell::RefCell::new(BufWriter::new(std::io::stdout().lock()));
    Some(match serve(&mut input, &out) {
        Ok(()) => 0,
        Err(_) => 2,
    })
}

// ------------------------------------------------------------------ format des échanges

fn put_u32(w: &mut impl Write, v: u32) -> std::io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

fn put_u64(w: &mut impl Write, v: u64) -> std::io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

fn put_bytes(w: &mut impl Write, b: &[u8]) -> std::io::Result<()> {
    put_u32(w, b.len() as u32)?;
    w.write_all(b)
}

fn put_str(w: &mut impl Write, s: &str) -> std::io::Result<()> {
    put_bytes(w, s.as_bytes())
}

/// Chemin tel quel (Windows : unités UTF-16, y compris celles qui ne forment pas du texte).
fn path_bytes(p: &Path) -> Vec<u8> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        p.as_os_str()
            .encode_wide()
            .flat_map(|u| u.to_le_bytes())
            .collect()
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::ffi::OsStrExt;
        p.as_os_str().as_bytes().to_vec()
    }
}

fn path_from(b: &[u8]) -> PathBuf {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        let w: Vec<u16> = b
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        PathBuf::from(std::ffi::OsString::from_wide(&w))
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::ffi::OsStringExt;
        PathBuf::from(std::ffi::OsString::from_vec(b.to_vec()))
    }
}

fn get<const N: usize>(r: &mut impl Read) -> std::io::Result<[u8; N]> {
    let mut b = [0u8; N];
    r.read_exact(&mut b)?;
    Ok(b)
}

fn get_u32(r: &mut impl Read) -> std::io::Result<u32> {
    Ok(u32::from_le_bytes(get::<4>(r)?))
}

fn get_u64(r: &mut impl Read) -> std::io::Result<u64> {
    Ok(u64::from_le_bytes(get::<8>(r)?))
}

/// Bloc d'octets de longueur annoncée (bornée : jamais une réservation d'après le fil).
fn get_bytes(r: &mut impl Read) -> std::io::Result<Vec<u8>> {
    let n = get_u32(r)? as usize;
    if n > 1 << 16 {
        return Err(std::io::Error::other("message trop long"));
    }
    let mut b = vec![0u8; n];
    r.read_exact(&mut b)?;
    Ok(b)
}

fn get_str(r: &mut impl Read) -> std::io::Result<String> {
    Ok(String::from_utf8_lossy(&get_bytes(r)?).into_owned())
}

fn request(kind: u8, sr: u32, fields: &[&[u8]]) -> Vec<u8> {
    let mut v = MAGIC.to_vec();
    v.push(kind);
    v.extend_from_slice(&sr.to_le_bytes());
    for f in fields {
        let _ = put_bytes(&mut v, f);
    }
    v
}

// ------------------------------------------------------------------ côté processus fils

type Out<'a, W> = &'a std::cell::RefCell<W>;

fn serve<W: Write>(input: &mut impl Read, out: Out<'_, W>) -> Result<()> {
    if get::<4>(input)? != MAGIC {
        bail!("demande inconnue");
    }
    let kind = get::<1>(input)?[0];
    let sr = get_u32(input)?;
    match kind {
        KIND_PAD => {
            let path = path_from(&get_bytes(input)?);
            serve_pad(&path, sr, out)
        }
        KIND_IMPORT => {
            let src = path_from(&get_bytes(input)?);
            let dir = path_from(&get_bytes(input)?);
            let file = get_str(input)?;
            serve_import(&src, &dir, &file, sr, out)
        }
        _ => bail!("demande inconnue"),
    }
}

/// Message d'avancement, au plus tous les `HEARTBEAT`.
fn heartbeat<W: Write>(out: Out<'_, W>) -> impl Fn(f64) + '_ {
    let last = std::cell::Cell::new(Instant::now());
    move |f: f64| {
        if last.get().elapsed() >= HEARTBEAT {
            last.set(Instant::now());
            let mut w = out.borrow_mut();
            let _ = w.write_all(&[MSG_PROGRESS]);
            let _ = w.write_all(&f.to_le_bytes());
            let _ = w.flush();
        }
    }
}

fn send_error<W: Write>(out: Out<'_, W>, msg: &str) -> Result<()> {
    let mut w = out.borrow_mut();
    w.write_all(&[MSG_ERROR])?;
    put_str(&mut *w, msg)?;
    w.flush()?;
    Ok(())
}

fn serve_pad<W: Write>(path: &Path, sr: u32, out: Out<'_, W>) -> Result<()> {
    let tick = heartbeat(out);
    // L'application réserve la place du son dès qu'elle connaît sa taille, et arrête ce
    // processus si elle manque : rien n'est décodé pour rien.
    let announce = |frames: u64| -> Result<()> {
        let mut w = out.borrow_mut();
        w.write_all(&[MSG_SIZE])?;
        put_u64(&mut *w, frames)?;
        w.flush()?;
        Ok(())
    };
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::soundboard::decode_stereo(path, sr, &tick, &announce)
    }));
    let data = match r {
        Ok(Ok(d)) => d,
        Ok(Err(e)) => return send_error(out, &format!("{e:#}")),
        Err(_) => return send_error(out, "fichier illisible (erreur du décodeur)"),
    };
    let mut w = out.borrow_mut();
    w.write_all(&[MSG_DATA])?;
    put_u64(&mut *w, (data.len() / 2) as u64)?;
    let mut buf = Vec::with_capacity(64 * 1024);
    for block in data.chunks(16 * 1024) {
        buf.clear();
        buf.extend(block.iter().flat_map(|x| x.to_le_bytes()));
        w.write_all(&buf)?;
    }
    w.flush()?;
    Ok(())
}

fn serve_import<W: Write>(
    src: &Path,
    dir: &Path,
    file: &str,
    sr: u32,
    out: Out<'_, W>,
) -> Result<()> {
    let tick = heartbeat(out);
    let progress = |f: f64| {
        tick(f);
        true
    };
    // Chaque message est envoyé (vidé) avant l'étape suivante : arrêté à tout moment, ce
    // processus ne laisse aucun fichier que l'application ne connaisse pas.
    let segment = |e: crate::import::SegmentEvent<'_>| {
        let mut w = out.borrow_mut();
        let _ = match e {
            crate::import::SegmentEvent::Naming(name) => w
                .write_all(&[MSG_NAMING])
                .and_then(|_| put_str(&mut *w, name)),
            crate::import::SegmentEvent::Done(name, frames) => w
                .write_all(&[MSG_SEGMENT])
                .and_then(|_| put_str(&mut *w, name))
                .and_then(|_| put_u64(&mut *w, frames)),
        };
        let _ = w.flush();
    };
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::import::import_file(src, dir, file, sr, &progress, &segment)
    }));
    match r {
        Ok(Ok(i)) => {
            let mut w = out.borrow_mut();
            w.write_all(&[MSG_DONE])?;
            put_str(&mut *w, &i.name)?;
            w.write_all(&[i.stereo as u8])?;
            w.flush()?;
            Ok(())
        }
        Ok(Err(e)) => send_error(out, &format!("{e:#}")),
        Err(_) => send_error(out, "fichier illisible (erreur du décodeur)"),
    }
}

// ------------------------------------------------------------------ côté application

/// Raison de l'arrêt du fils par l'application.
const STOP_NONE: u8 = 0;
const STOP_TIMEOUT: u8 = 1;
const STOP_CANCEL: u8 = 2;

/// Lecture de la sortie du fils : chaque octet reçu repousse l'échéance du chien de garde.
struct Activity {
    inner: ChildStdout,
    t0: Instant,
    last_ms: Arc<AtomicU64>,
}

impl Read for Activity {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.last_ms
            .store(self.t0.elapsed().as_millis() as u64, Ordering::Relaxed);
        Ok(n)
    }
}

/// Processus fils en cours.
struct Running {
    child: Arc<Mutex<Child>>,
    out: BufReader<Activity>,
    done: Arc<AtomicBool>,
    stopped: Arc<AtomicU8>,
    watchdog: Option<std::thread::JoinHandle<()>>,
    #[cfg(windows)]
    _job: Option<job::Job>,
}

impl Running {
    /// Lance le fils et lui envoie la demande. `None` : impossible (on décode alors dans
    /// le processus).
    fn start(req: &[u8], mem_limit: u64, cancel: Option<Arc<AtomicBool>>) -> Option<Self> {
        let exe = std::env::current_exe().ok()?;
        let mut cmd = Command::new(exe);
        cmd.arg(WORKER_ARG)
            .env(CHILD_ENV, "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS : ni console, ni concurrence
            // avec l'audio.
            cmd.creation_flags(0x0800_0000 | 0x0000_4000);
        }
        #[cfg(windows)]
        let job = job::Job::new(mem_limit);
        #[cfg(not(windows))]
        let _ = mem_limit;
        let mut child = cmd.spawn().ok()?;
        // Le fils attend la demande avant de lire quoi que ce soit : il est placé dans le
        // Job (mémoire bornée) avant de toucher au fichier.
        #[cfg(windows)]
        let job = job.filter(|j| j.assign(&child));
        let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        };
        if stdin.write_all(req).and_then(|_| stdin.flush()).is_err() {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        drop(stdin);
        let t0 = Instant::now();
        let last_ms = Arc::new(AtomicU64::new(0));
        let child = Arc::new(Mutex::new(child));
        let done = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicU8::new(STOP_NONE));
        let watchdog = {
            let (child, done, stopped, last_ms) = (
                child.clone(),
                done.clone(),
                stopped.clone(),
                last_ms.clone(),
            );
            std::thread::Builder::new()
                .name("tracksplus-decode-watch".into())
                .spawn(move || {
                    while !done.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(100));
                        let now = t0.elapsed().as_millis() as u64;
                        let silent = now.saturating_sub(last_ms.load(Ordering::Relaxed));
                        let why = if cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed)) {
                            STOP_CANCEL
                        } else if silent > SILENCE_TIMEOUT.as_millis() as u64 {
                            STOP_TIMEOUT
                        } else {
                            continue;
                        };
                        stopped.store(why, Ordering::Relaxed);
                        let _ = child.lock().kill();
                        break;
                    }
                })
                .ok()
        };
        Some(Self {
            child,
            out: BufReader::with_capacity(
                256 * 1024,
                Activity {
                    inner: stdout,
                    t0,
                    last_ms,
                },
            ),
            done,
            stopped,
            watchdog,
            #[cfg(windows)]
            _job: job,
        })
    }

    /// Arrête le fils s'il tourne encore et rend la raison d'un arrêt forcé.
    fn finish(mut self) -> u8 {
        self.done.store(true, Ordering::Relaxed);
        if let Some(w) = self.watchdog.take() {
            let _ = w.join();
        }
        let mut c = self.child.lock();
        if !matches!(c.try_wait(), Ok(Some(_))) {
            let _ = c.kill();
        }
        let _ = c.wait();
        self.stopped.load(Ordering::Relaxed)
    }
}

/// Message d'un fils qui s'est arrêté sans répondre.
fn died(stopped: u8) -> String {
    if stopped == STOP_TIMEOUT {
        "fichier illisible (décodage bloqué, arrêté)".into()
    } else {
        "fichier illisible : le décodage s'est arrêté (fichier abîmé ou trop gros pour la \
         mémoire disponible)"
            .into()
    }
}

/// Mémoire physique de l'ordinateur (octets), si on sait la lire.
pub fn physical_memory() -> Option<u64> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        // SAFETY : structure initialisée avec sa taille, comme l'exige l'API.
        unsafe {
            let mut m: MEMORYSTATUSEX = std::mem::zeroed();
            m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
            (GlobalMemoryStatusEx(&mut m) != 0).then_some(m.ullTotalPhys)
        }
    }
    #[cfg(not(windows))]
    None
}

/// Limite mémoire du fils : ce dont un décodage légitime a besoin, jamais plus de la moitié
/// de la mémoire de l'ordinateur.
fn child_limit(need: u64) -> u64 {
    let half = physical_memory().map_or(u64::MAX, |m| m / 2);
    need.min(half.max(1 << 30))
}

/// Son d'un pad décodé (stéréo entrelacé à `sr`) dans un processus fils, dont la mémoire
/// est bornée à `need`. `accept(trames)` est appelé dès que le fils annonce la taille du
/// son (avant qu'il ne le décode), puis avec sa taille réelle avant toute réservation ici
/// (taille maximale, mémoire réservée aux sons) ; un refus arrête le fils aussitôt et est
/// rendu tel quel. `None` : décodage isolé indisponible.
pub fn decode_pad(
    path: &Path,
    sr: u32,
    need: u64,
    accept: &dyn Fn(u64) -> Result<(), String>,
) -> Option<Result<Vec<f32>, String>> {
    if !enabled() {
        return None;
    }
    let req = request(KIND_PAD, sr, &[&path_bytes(path)]);
    let mut run = Running::start(&req, child_limit(need), None)?;
    let r = read_pad(&mut run.out, accept);
    let stopped = run.finish();
    Some(r.unwrap_or_else(|e| Err(e.unwrap_or_else(|| died(stopped)))))
}

/// Lit la réponse d'un fils de décodage de pad. `Err(None)` : flux interrompu.
fn read_pad(
    r: &mut impl Read,
    accept: &dyn Fn(u64) -> Result<(), String>,
) -> Result<Result<Vec<f32>, String>, Option<String>> {
    loop {
        let tag = get::<1>(r).map_err(|_| None)?[0];
        match tag {
            MSG_PROGRESS => {
                get::<8>(r).map_err(|_| None)?;
            }
            MSG_SIZE => {
                let frames = get_u64(r).map_err(|_| None)?;
                if let Err(e) = accept(frames) {
                    return Ok(Err(e));
                }
            }
            MSG_ERROR => return Ok(Err(get_str(r).map_err(|_| None)?)),
            MSG_DATA => {
                let frames = get_u64(r).map_err(|_| None)?;
                if let Err(e) = accept(frames) {
                    return Ok(Err(e));
                }
                let n = frames
                    .checked_mul(2)
                    .and_then(|n| usize::try_from(n).ok())
                    .ok_or_else(|| Some("son trop long".to_string()))?;
                let mut data: Vec<f32> = Vec::new();
                if data.try_reserve_exact(n).is_err() {
                    return Ok(Err("mémoire insuffisante pour charger ce son".into()));
                }
                let mut buf = vec![0u8; 256 * 1024];
                while data.len() < n {
                    let k = ((n - data.len()) * 4).min(buf.len());
                    r.read_exact(&mut buf[..k]).map_err(|_| None)?;
                    data.extend(
                        buf[..k]
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .map(|c| f32::from_le_bytes(*c)),
                    );
                }
                return Ok(Ok(data));
            }
            _ => return Err(Some("réponse du décodeur illisible".into())),
        }
    }
}

/// Import d'un fichier (voir `import::import_file`) dans un processus fils. `None` :
/// décodage isolé indisponible.
pub fn import(
    src: &Path,
    audio_dir: &Path,
    file: &str,
    sr: u32,
    progress: &dyn Fn(f64) -> bool,
    cancel: &Arc<AtomicBool>,
) -> Option<Result<crate::import::Imported>> {
    if !enabled() {
        return None;
    }
    let req = request(
        KIND_IMPORT,
        sr,
        &[&path_bytes(src), &path_bytes(audio_dir), file.as_bytes()],
    );
    // Tout se fait par blocs : peu de mémoire, sauf un rééchantillonneur de fréquence
    // première (quelques dizaines de Mo).
    let existing = existing_names(audio_dir, file);
    let mut run = Running::start(&req, child_limit(1 << 30), Some(cancel.clone()))?;
    let mut segments = Vec::new();
    let mut naming = None;
    let r = read_import(
        &mut run.out,
        &existing,
        file,
        progress,
        &mut segments,
        &mut naming,
    );
    let stopped = run.finish();
    Some(match r {
        Some(r) => r,
        None => {
            // Fils arrêté en cours de route : ses fichiers sont retirés ici, y compris
            // celui qu'il renommait (nom annoncé libre juste avant).
            crate::import::remove_segments(audio_dir, file, &segments);
            if let Some(n) = naming {
                let _ = std::fs::remove_file(audio_dir.join(n));
            }
            if stopped == STOP_CANCEL || cancel.load(Ordering::Relaxed) {
                Err(crate::import::Cancelled.into())
            } else {
                Err(anyhow!(died(stopped)))
            }
        }
    })
}

/// Fichiers du dossier Audio qui pourraient porter le nom d'un segment de `file` (en
/// minuscules : Windows ne distingue pas la casse), relevés avant de lancer le fils.
fn existing_names(audio_dir: &Path, file: &str) -> HashSet<String> {
    let stem = file.trim_end_matches(".wav").to_lowercase();
    std::fs::read_dir(audio_dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_lowercase())
                .filter(|n| n.starts_with(&stem))
                .collect()
        })
        .unwrap_or_default()
}

/// Lit les messages d'un fils d'import. `None` : flux interrompu. Les noms de fichiers
/// reçus (`segments`, et `naming` pour celui en cours de renommage) sont ceux que
/// l'application retirera si le fils s'arrête : seul le nom attendu du segment suivant,
/// absent du dossier avant l'import (`existing`, en minuscules), est accepté (un fils
/// détourné par un fichier piégé ne fait jamais retirer une prise existante).
fn read_import(
    r: &mut impl Read,
    existing: &HashSet<String>,
    file: &str,
    progress: &dyn Fn(f64) -> bool,
    segments: &mut Vec<(String, u64)>,
    naming: &mut Option<String>,
) -> Option<Result<crate::import::Imported>> {
    loop {
        let tag = get::<1>(r).ok()?[0];
        let expected = || crate::recorder::segment_name(file, segments.len() + 1);
        match tag {
            MSG_PROGRESS => {
                progress(f64::from_le_bytes(get::<8>(r).ok()?));
            }
            MSG_NAMING => {
                let name = get_str(r).ok()?;
                // Un fichier présent avant l'import n'est jamais le sien.
                if name != expected() || existing.contains(&name.to_lowercase()) {
                    return None;
                }
                *naming = Some(name);
            }
            MSG_SEGMENT => {
                let name = get_str(r).ok()?;
                let frames = get_u64(r).ok()?;
                if naming.as_deref() != Some(name.as_str()) {
                    return None;
                }
                *naming = None;
                segments.push((name, frames));
            }
            MSG_DONE => {
                let name = get_str(r).ok()?;
                let stereo = get::<1>(r).ok()?[0] != 0;
                return Some(Ok(crate::import::Imported {
                    name,
                    segments: std::mem::take(segments),
                    stereo,
                }));
            }
            MSG_ERROR => return Some(Err(anyhow!(get_str(r).ok()?))),
            _ => return None,
        }
    }
}

#[cfg(windows)]
mod job {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    /// Job Object : mémoire du fils bornée, fils tué quand le Job est fermé.
    pub struct Job(HANDLE);

    // SAFETY : un HANDLE de Job s'utilise depuis n'importe quel thread.
    unsafe impl Send for Job {}

    impl Job {
        pub fn new(limit: u64) -> Option<Self> {
            // SAFETY : appels documentés, structure initialisée à zéro puis remplie ; le
            // HANDLE est fermé en cas d'échec.
            unsafe {
                let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if h.is_null() {
                    return None;
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_PROCESS_MEMORY
                    | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
                    | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
                info.ProcessMemoryLimit = usize::try_from(limit).unwrap_or(usize::MAX);
                let ok = SetInformationJobObject(
                    h,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const core::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if ok == 0 {
                    CloseHandle(h);
                    return None;
                }
                Some(Job(h))
            }
        }

        pub fn assign(&self, child: &std::process::Child) -> bool {
            use std::os::windows::io::AsRawHandle;
            // SAFETY : les deux HANDLE sont valides tant que `self` et `child` vivent.
            unsafe { AssignProcessToJobObject(self.0, child.as_raw_handle() as HANDLE) != 0 }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY : HANDLE obtenu de CreateJobObjectW, fermé une seule fois.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_roundtrip_through_the_worker_protocol() {
        // Demande de pad : chemin avec accents et espaces, lu à l'identique.
        let p = Path::new("C:/Sons/Générique début.wav");
        let req = request(KIND_PAD, 48000, &[&path_bytes(p)]);
        let mut r = &req[..];
        assert_eq!(get::<4>(&mut r).unwrap(), MAGIC);
        assert_eq!(get::<1>(&mut r).unwrap()[0], KIND_PAD);
        assert_eq!(get_u32(&mut r).unwrap(), 48000);
        assert_eq!(path_from(&get_bytes(&mut r).unwrap()), p);
        // Demande inconnue : refusée.
        let out = std::cell::RefCell::new(Vec::new());
        assert!(serve(&mut &b"XXXX"[..], &out).is_err());
    }

    /// Un fils qui décode un vrai fichier : réponse relue en entier par l'application, et
    /// le refus (mémoire pleine) est respecté avant toute réservation.
    #[test]
    fn pad_response_is_read_back() {
        let dir = std::env::temp_dir().join(format!("tp-iso-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        for i in 0..3000 {
            w.write_sample((i % 100) as i16 * 100).unwrap();
        }
        w.finalize().unwrap();
        let req = request(KIND_PAD, 48000, &[&path_bytes(&path)]);
        let out = std::cell::RefCell::new(Vec::new());
        serve(&mut &req[..], &out).unwrap();
        let bytes = out.into_inner();
        let calls = std::cell::Cell::new(0);
        let data = read_pad(&mut &bytes[..], &|n| {
            assert_eq!(n, 3000);
            calls.set(calls.get() + 1);
            Ok(())
        })
        .unwrap()
        .unwrap();
        assert_eq!(data.len(), 6000);
        assert_eq!(data[200], data[201]);
        // Taille annoncée dès l'ouverture, puis taille réelle avec les données.
        assert_eq!(calls.get(), 2);
        let refused = read_pad(&mut &bytes[..], &|_| Err("pleine".into())).unwrap();
        assert_eq!(refused.unwrap_err(), "pleine");
        // Le refus arrive sur l'annonce, avant les données : le fils est arrêté sans
        // avoir envoyé le son.
        assert_eq!(bytes[0], MSG_SIZE);
        // Flux coupé au milieu des données : signalé comme un arrêt du fils.
        assert!(read_pad(&mut &bytes[..bytes.len() / 2], &|_| Ok(())).is_err());
        // Fichier illisible : erreur transmise.
        std::fs::write(&path, b"pas du son").unwrap();
        let out = std::cell::RefCell::new(Vec::new());
        serve(&mut &req[..], &out).unwrap();
        let bytes = out.into_inner();
        assert!(read_pad(&mut &bytes[..], &|_| Ok(())).unwrap().is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Fils qui s'arrête sans répondre (ici l'exécutable des tests, qui ne connaît pas
    /// `WORKER_ARG`) : simple erreur sur le pad, l'application continue.
    #[test]
    fn dead_worker_is_an_error_not_a_crash() {
        // Jamais de lancement en cascade si l'exécutable des tests acceptait l'argument.
        if std::env::var_os(CHILD_ENV).is_some() {
            return;
        }
        let req = request(KIND_PAD, 48000, &[&path_bytes(Path::new("x.wav"))]);
        if let Some(mut run) = Running::start(&req, 1 << 30, None) {
            let r = read_pad(&mut run.out, &|_| Ok(()));
            let stopped = run.finish();
            assert!(matches!(r, Err(None)));
            assert_eq!(stopped, STOP_NONE);
        }
        assert!(died(STOP_NONE).contains("illisible"));
        assert!(died(STOP_TIMEOUT).contains("bloqué"));
    }

    #[test]
    fn import_response_is_read_back() {
        let dir = std::env::temp_dir().join(format!("tp-isoimp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("chanson.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 44100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&src, spec).unwrap();
        for i in 0..20000 {
            w.write_sample((i % 50) as i16).unwrap();
        }
        w.finalize().unwrap();
        let audio = dir.join("Audio");
        let req = request(
            KIND_IMPORT,
            48000,
            &[&path_bytes(&src), &path_bytes(&audio), b"chanson_001.wav"],
        );
        let out = std::cell::RefCell::new(Vec::new());
        serve(&mut &req[..], &out).unwrap();
        let bytes = out.into_inner();
        let file = "chanson_001.wav";
        let none = HashSet::new();
        let mut segs = Vec::new();
        let i = read_import(
            &mut &bytes[..],
            &none,
            file,
            &|_| true,
            &mut segs,
            &mut None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(i.name, "chanson");
        assert!(i.stereo);
        assert_eq!(i.segments.len(), 1);
        assert_eq!(i.segments[0].0, "chanson_001.wav");
        assert_eq!(i.frames(), (10000u64 * 48000).div_ceil(44100));
        assert!(audio.join("chanson_001.wav").exists());
        // Flux interrompu : `None` (le fils s'est arrêté).
        let mut naming = None;
        let cut = read_import(
            &mut &bytes[..3],
            &none,
            file,
            &|_| true,
            &mut segs,
            &mut naming,
        );
        assert!(cut.is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Fils arrêté entre l'annonce d'un nom et le message du segment terminé : le nom est
    /// connu (le fichier sera retiré). Un nom inattendu, ou déjà pris avant l'import, ou un
    /// segment jamais annoncé, est refusé : un fils détourné ne fait retirer aucune prise.
    #[test]
    fn import_segment_names_are_checked() {
        let file = "concert_001.wav";
        let msg = |tag: u8, name: &str, frames: Option<u64>| {
            let mut v = vec![tag];
            put_str(&mut v, name).unwrap();
            if let Some(f) = frames {
                put_u64(&mut v, f).unwrap();
            }
            v
        };
        let read = |bytes: &[u8], existing: &HashSet<String>| {
            let (mut segs, mut naming) = (Vec::new(), None);
            let r = read_import(
                &mut &bytes[..],
                existing,
                file,
                &|_| true,
                &mut segs,
                &mut naming,
            );
            (r.is_none(), segs, naming)
        };
        let none = HashSet::new();
        // Premier segment terminé, second annoncé puis arrêt.
        let mut s = msg(MSG_NAMING, "concert_001.wav", None);
        s.extend(msg(MSG_SEGMENT, "concert_001.wav", Some(10)));
        s.extend(msg(MSG_NAMING, "concert_001_p2.wav", None));
        let (stopped, segs, naming) = read(&s, &none);
        assert!(stopped);
        assert_eq!(segs, vec![("concert_001.wav".to_string(), 10)]);
        assert_eq!(naming.as_deref(), Some("concert_001_p2.wav"));
        // Nom qui n'est pas celui du segment suivant.
        let (_, _, naming) = read(&msg(MSG_NAMING, "Prise_001.wav", None), &none);
        assert_eq!(naming, None);
        // Nom déjà présent avant l'import (casse indifférente).
        let taken: HashSet<String> = ["concert_001.wav".to_string()].into();
        let (_, _, naming) = read(&msg(MSG_NAMING, "concert_001.wav", None), &taken);
        assert_eq!(naming, None);
        // Segment terminé sans annonce.
        let (_, segs, _) = read(&msg(MSG_SEGMENT, "concert_001.wav", Some(10)), &none);
        assert!(segs.is_empty());
        // Relevé du dossier : en minuscules, limité aux noms de cet import.
        let dir = std::env::temp_dir().join(format!("tp-isonames-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Concert_001_P3.wav"), b"").unwrap();
        std::fs::write(dir.join("autre.wav"), b"").unwrap();
        let found = existing_names(&dir, file);
        assert_eq!(found, ["concert_001_p3.wav".to_string()].into());
        let _ = std::fs::remove_dir_all(dir);
    }
}
