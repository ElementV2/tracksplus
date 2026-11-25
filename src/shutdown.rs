//! Sûreté de fin de processus : arrêt, redémarrage ou fermeture de session Windows, erreur
//! fatale de l'interface ou du rendu. Une prise en cours n'est jamais coupée sans que ses
//! fichiers soient finalisés (en-têtes WAV à jour) ; après une erreur fatale, l'application
//! se relance d'elle-même dans la session en cours.
//!
//! État partagé par des atomiques : la procédure de fenêtre (fin de session Windows) et la
//! fin de `main` n'ont pas accès à l'application.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Prise en cours ou en finalisation (tenu à jour par l'interface, toutes les 100 ms).
static TAKE_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
/// Fermeture de l'application voulue par l'utilisateur : jamais de relance ensuite.
static EXIT_REQUESTED: AtomicBool = AtomicBool::new(false);
/// Fermeture propre de l'application déjà commencée (une seule fois).
static SHUTDOWN_STARTED: AtomicBool = AtomicBool::new(false);
/// Windows a demandé l'arrêt pendant une prise et on l'a refusé (message à afficher).
static SHUTDOWN_REFUSED: AtomicBool = AtomicBool::new(false);
/// Windows prévient d'un arrêt : sauvegarder la session tout de suite.
static SAVE_REQUESTED: AtomicBool = AtomicBool::new(false);
/// Fin de session Windows confirmée : les prises ont été finalisées en urgence.
static SESSION_ENDED: AtomicBool = AtomicBool::new(false);
/// Carte graphique perdue (pilote réinitialisé…) : l'affichage ne reviendra pas.
static GPU_LOST: AtomicBool = AtomicBool::new(false);

/// Délais de la fin d'urgence pendant la fin de session Windows (qui n'attend que quelques
/// secondes) : écriture des tampons des prises, puis copie de sécurité.
const END_SESSION_WRITERS: Duration = Duration::from_secs(4);
const END_SESSION_BACKUPS: Duration = Duration::from_secs(1);

pub fn set_take_in_progress(on: bool) {
    TAKE_IN_PROGRESS.store(on, Ordering::Release);
}

pub fn take_in_progress() -> bool {
    TAKE_IN_PROGRESS.load(Ordering::Acquire)
}

pub fn mark_exit_requested() {
    EXIT_REQUESTED.store(true, Ordering::Release);
}

pub fn exit_requested() -> bool {
    EXIT_REQUESTED.load(Ordering::Acquire)
}

/// `true` la première fois seulement : la fermeture propre ne se fait qu'une fois, qu'elle
/// vienne de `on_exit` ou de la destruction de l'application pendant une erreur fatale.
pub fn begin_shutdown() -> bool {
    !SHUTDOWN_STARTED.swap(true, Ordering::AcqRel)
}

pub fn take_shutdown_refused() -> bool {
    SHUTDOWN_REFUSED.swap(false, Ordering::AcqRel)
}

pub fn take_save_request() -> bool {
    SAVE_REQUESTED.swap(false, Ordering::AcqRel)
}

pub fn session_ended() -> bool {
    SESSION_ENDED.load(Ordering::Acquire)
}

pub fn set_gpu_lost() {
    GPU_LOST.store(true, Ordering::Release);
}

pub fn take_gpu_lost() -> bool {
    GPU_LOST.swap(false, Ordering::AcqRel)
}

/// Fin de session Windows confirmée (arrêt forcé ou accepté) : le processus peut être tué
/// dès le retour. Les prises en cours écrivent leurs tampons et ferment leurs fichiers ;
/// le journal reste, elles sont réintégrées à la session au prochain lancement.
fn end_session_now() {
    crate::recorder::emergency_finish(END_SESSION_WRITERS);
    crate::recorder::wait_backups(END_SESSION_BACKUPS);
    SESSION_ENDED.store(true, Ordering::Release);
}

// ------------------------------------------------------------------ relance après incident

/// Argument d'un lancement fait par TracksPlus lui-même après une erreur fatale.
pub const RELAUNCH_ARG: &str = "--reprise-apres-incident";
/// Nombre de relances successives (transmis au processus relancé).
const RELAUNCH_ENV: &str = "TRACKSPLUS_REPRISES";
/// Au-delà, plus de relance : l'erreur se reproduit, inutile de boucler.
const MAX_RELAUNCHES: u32 = 5;
/// Une erreur survenue moins longtemps après le lancement n'entraîne pas de relance (erreur
/// au démarrage : elle se reproduirait aussitôt).
const MIN_UPTIME: Duration = Duration::from_secs(60);

/// Relance après une erreur fatale : seulement si l'utilisateur n'avait pas demandé à
/// quitter, si l'application tournait depuis un moment, et sans boucle infinie.
pub fn should_relaunch(uptime: Duration, relaunches: u32, exit_requested: bool) -> bool {
    !exit_requested && uptime >= MIN_UPTIME && relaunches < MAX_RELAUNCHES
}

/// Ce lancement est une relance automatique après une erreur fatale.
pub fn relaunched() -> bool {
    std::env::args().any(|a| a == RELAUNCH_ARG)
}

/// Relances successives déjà faites avant ce processus.
pub fn relaunch_count() -> u32 {
    parse_relaunch_count(std::env::var(RELAUNCH_ENV).ok().as_deref())
}

fn parse_relaunch_count(v: Option<&str>) -> u32 {
    v.and_then(|s| s.trim().parse().ok()).unwrap_or(0)
}

/// Relance TracksPlus (le verrou d'instance unique doit déjà être libéré). La session est
/// rouverte au démarrage (journal de récupération, ou dernière session utilisée).
pub fn relaunch() -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    std::process::Command::new(exe)
        .arg(RELAUNCH_ARG)
        .env(RELAUNCH_ENV, (relaunch_count() + 1).to_string())
        .spawn()
        .map(|_| ())
}

// ------------------------------------------------------------------ Windows

/// Branche la fenêtre principale sur les messages de fin de session de Windows (ni winit
/// ni eframe ne les traitent : Windows fermerait l'application en pleine prise).
#[cfg(windows)]
pub fn install(hwnd: isize) {
    win::install(hwnd);
}

#[cfg(not(windows))]
pub fn install(_hwnd: isize) {}

/// Raison affichée par Windows à l'écran d'arrêt tant qu'une prise est en cours. À appeler
/// depuis le thread de l'interface (celui de la fenêtre).
#[cfg(windows)]
pub fn sync_block_reason(recording: bool) {
    win::sync_block_reason(recording);
}

#[cfg(not(windows))]
pub fn sync_block_reason(_recording: bool) {}

#[cfg(windows)]
mod win {
    use super::*;
    use std::sync::atomic::AtomicIsize;
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::Shutdown::{
        ShutdownBlockReasonCreate, ShutdownBlockReasonDestroy,
    };
    use windows_sys::Win32::System::Threading::SetProcessShutdownParameters;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallWindowProcW, DefWindowProcW, GWLP_WNDPROC, SetWindowLongPtrW, WM_ENDSESSION,
        WM_QUERYENDSESSION, WNDPROC,
    };

    /// Fenêtre principale (0 = pas encore branchée).
    static HWND_MAIN: AtomicIsize = AtomicIsize::new(0);
    /// Procédure de fenêtre d'origine (celle de winit).
    static PREV_PROC: AtomicIsize = AtomicIsize::new(0);
    /// Raison de blocage de l'arrêt actuellement déclarée à Windows.
    static REASON_SET: AtomicBool = AtomicBool::new(false);

    const REASON: &str =
        "Enregistrement en cours : arrêtez la prise dans TracksPlus avant d'éteindre";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn install(hwnd: isize) {
        if hwnd == 0 || PREV_PROC.load(Ordering::Acquire) != 0 {
            return;
        }
        // SAFETY : `hwnd` est la fenêtre principale, créée par ce thread ; la nouvelle
        // procédure appelle toujours l'ancienne (sous-classement classique).
        unsafe {
            // Prévenus parmi les premiers : si on refuse l'arrêt, les autres applications
            // n'ont pas encore été fermées.
            SetProcessShutdownParameters(0x3FF, 0);
            let prev = SetWindowLongPtrW(hwnd as HWND, GWLP_WNDPROC, wndproc as *const () as isize);
            if prev != 0 {
                PREV_PROC.store(prev, Ordering::Release);
                HWND_MAIN.store(hwnd, Ordering::Release);
            }
        }
    }

    fn set_reason(hwnd: HWND) {
        let text = wide(REASON);
        // SAFETY : chaîne terminée par un zéro, vivante pendant l'appel ; thread de la fenêtre.
        if unsafe { ShutdownBlockReasonCreate(hwnd, text.as_ptr()) } != 0 {
            REASON_SET.store(true, Ordering::Release);
        }
    }

    pub fn sync_block_reason(recording: bool) {
        let hwnd = HWND_MAIN.load(Ordering::Acquire);
        if hwnd == 0 || recording == REASON_SET.load(Ordering::Acquire) {
            return;
        }
        if recording {
            set_reason(hwnd as HWND);
        } else {
            // SAFETY : fenêtre de ce thread.
            unsafe { ShutdownBlockReasonDestroy(hwnd as HWND) };
            REASON_SET.store(false, Ordering::Release);
        }
    }

    /// `0` : refus de l'arrêt (WM_QUERYENDSESSION) ; `None` : laisser faire winit.
    fn handle(hwnd: HWND, msg: u32, wparam: WPARAM) -> Option<LRESULT> {
        match msg {
            WM_QUERYENDSESSION => {
                // Dans tous les cas, la session est sauvegardée sans attendre.
                SAVE_REQUESTED.store(true, Ordering::Release);
                if take_in_progress() {
                    // Écran d'arrêt de Windows : « TracksPlus empêche l'arrêt » avec la
                    // raison ; l'utilisateur peut annuler, la prise continue.
                    set_reason(hwnd);
                    SHUTDOWN_REFUSED.store(true, Ordering::Release);
                    return Some(0);
                }
                None
            }
            // Arrêt confirmé (forcé, ou accepté) : finaliser avant d'être tué.
            WM_ENDSESSION if wparam != 0 => {
                end_session_now();
                None
            }
            _ => None,
        }
    }

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        // Aucune panique ne doit traverser une procédure de fenêtre (arrêt brutal).
        if matches!(msg, WM_QUERYENDSESSION | WM_ENDSESSION)
            && let Ok(Some(r)) = std::panic::catch_unwind(|| handle(hwnd, msg, wparam))
        {
            return r;
        }
        let prev = PREV_PROC.load(Ordering::Acquire);
        // SAFETY : `prev` est la procédure d'origine renvoyée par SetWindowLongPtrW.
        unsafe {
            if prev == 0 {
                return DefWindowProcW(hwnd, msg, wparam, lparam);
            }
            let prev: WNDPROC = std::mem::transmute::<isize, WNDPROC>(prev);
            CallWindowProcW(prev, hwnd, msg, wparam, lparam)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relaunch_only_after_a_real_session_and_without_looping() {
        let long = Duration::from_secs(3600);
        assert!(should_relaunch(long, 0, false));
        assert!(should_relaunch(long, MAX_RELAUNCHES - 1, false));
        // L'utilisateur avait demandé à quitter : jamais de relance.
        assert!(!should_relaunch(long, 0, true));
        // Erreur au démarrage : elle se reproduirait, pas de relance.
        assert!(!should_relaunch(Duration::from_secs(5), 0, false));
        // Trop de relances successives : on s'arrête.
        assert!(!should_relaunch(long, MAX_RELAUNCHES, false));
    }

    #[test]
    fn relaunch_count_is_read_safely() {
        assert_eq!(parse_relaunch_count(None), 0);
        assert_eq!(parse_relaunch_count(Some("3")), 3);
        assert_eq!(parse_relaunch_count(Some(" 2 ")), 2);
        assert_eq!(parse_relaunch_count(Some("n'importe quoi")), 0);
    }

    #[test]
    fn shutdown_happens_once() {
        // (Seul test à utiliser ce drapeau global.)
        let first = begin_shutdown();
        assert!(!begin_shutdown());
        assert!(first);
    }
}
