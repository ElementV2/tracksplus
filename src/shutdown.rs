//! Sûreté de fin de processus : arrêt, redémarrage ou fermeture de session Windows, erreur
//! fatale de l'interface ou du rendu. Une prise en cours n'est jamais coupée sans que ses
//! fichiers soient finalisés (en-têtes WAV à jour) ; après une erreur fatale, l'application
//! se relance d'elle-même dans la session en cours.
//!
//! État partagé par des atomiques : la procédure de fenêtre (fin de session Windows) et la
//! fin de `main` n'ont pas accès à l'application.

use std::sync::Mutex;
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
/// Windows prévient d'un arrêt : sauvegarder la session tout de suite (au prochain passage
/// de l'interface, ou à la fin de session si elle n'a pas pu passer d'ici là).
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

/// Écriture différée d'une sauvegarde (état figé au moment où elle a été préparée).
pub type PendingWrite = Box<dyn FnOnce() + Send>;

/// Sauvegardes prêtes à écrire, tenues à jour par l'interface tant que la session ou le
/// soundboard ont des modifications non sauvegardées : si Windows ferme la session avant
/// que l'interface ait pu sauvegarder, elles sont écrites par la fin de session elle-même.
#[derive(Default)]
struct PendingSaves {
    session: Option<PendingWrite>,
    board: Option<PendingWrite>,
}

static PENDING: Mutex<PendingSaves> = Mutex::new(PendingSaves {
    session: None,
    board: None,
});

fn with_pending(f: impl FnOnce(&mut PendingSaves)) {
    let mut p = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut p);
}

/// Sauvegarde de la session à écrire en cas de fin de session Windows (`None` : rien à
/// sauvegarder). Ne doit jamais écraser un état plus récent déjà écrit.
pub fn set_pending_session_save(w: Option<PendingWrite>) {
    with_pending(|p| p.session = w);
}

/// Idem pour le soundboard.
pub fn set_pending_board_save(w: Option<PendingWrite>) {
    with_pending(|p| p.board = w);
}

/// Écrit les sauvegardes en attente (session d'abord : le soundboard d'une session neuve
/// a besoin de son dossier). Sans jamais bloquer ni paniquer (procédure de fenêtre).
fn write_pending_saves() {
    let taken = match PENDING.try_lock() {
        Ok(mut p) => std::mem::take(&mut *p),
        Err(std::sync::TryLockError::Poisoned(e)) => std::mem::take(&mut *e.into_inner()),
        Err(std::sync::TryLockError::WouldBlock) => return,
    };
    for w in [taken.session, taken.board].into_iter().flatten() {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(w));
    }
}

/// Fin de session Windows confirmée (arrêt forcé ou accepté) : le processus peut être tué
/// dès le retour. Les prises en cours écrivent leurs tampons et ferment leurs fichiers ;
/// le journal reste, elles sont réintégrées à la session au prochain lancement. Si
/// l'interface n'a pas encore sauvegardé depuis l'annonce de l'arrêt, les sauvegardes
/// préparées sont écrites ici.
fn end_session_now() {
    // Avant tout : une erreur pendant la fermeture de Windows ne relance pas TracksPlus.
    mark_exit_requested();
    crate::recorder::emergency_finish(END_SESSION_WRITERS, END_SESSION_WRITERS);
    if take_save_request() {
        write_pending_saves();
    }
    crate::recorder::wait_backups(END_SESSION_BACKUPS, END_SESSION_BACKUPS);
    SESSION_ENDED.store(true, Ordering::Release);
}

/// Message de fin de session de Windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionMsg {
    /// WM_QUERYENDSESSION : Windows demande s'il peut fermer la session.
    QueryEnd,
    /// WM_ENDSESSION : `ending` vrai si la session se ferme vraiment (sinon, annulée).
    End { ending: bool },
}

/// Réponse à un message de fin de session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionAction {
    /// Prise en cours ou en finalisation : arrêt refusé (avec la raison affichée par
    /// Windows), session sauvegardée.
    Refuse,
    /// Rien d'important en cours : arrêt accepté, session sauvegardée sans attendre.
    AllowAndSave,
    /// Arrêt confirmé : prises finalisées en urgence avant d'être tué.
    FinishNow,
    /// Arrêt annulé : rien à faire.
    Nothing,
}

/// `busy` : prise en cours ou en finalisation.
fn session_action(msg: SessionMsg, busy: bool) -> SessionAction {
    match msg {
        SessionMsg::QueryEnd if busy => SessionAction::Refuse,
        SessionMsg::QueryEnd => SessionAction::AllowAndSave,
        SessionMsg::End { ending: true } => SessionAction::FinishNow,
        SessionMsg::End { ending: false } => SessionAction::Nothing,
    }
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
/// Après une vraie session (au moins ce temps sans incident), les incidents précédents ne
/// comptent plus : seuls les incidents rapprochés épuisent les relances (sur un festival de
/// plusieurs jours, quelques incidents espacés de plusieurs heures ne bloquent rien).
const RELAUNCH_RESET_UPTIME: Duration = Duration::from_secs(10 * 60);

/// Circonstances de la fin du processus sur une erreur fatale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndContext {
    /// Temps écoulé depuis le lancement de ce processus.
    pub uptime: Duration,
    /// Relances successives déjà faites avant ce processus.
    pub relaunches: u32,
    /// L'utilisateur avait demandé à quitter.
    pub exit_requested: bool,
    /// Windows ferme la session (arrêt, redémarrage, déconnexion).
    pub session_ended: bool,
}

impl EndContext {
    pub fn now(uptime: Duration) -> Self {
        Self {
            uptime,
            relaunches: relaunch_count(),
            exit_requested: exit_requested(),
            session_ended: session_ended(),
        }
    }

    /// Relances rapprochées déjà faites (remises à zéro après une vraie session).
    fn recent_relaunches(&self) -> u32 {
        if self.uptime >= RELAUNCH_RESET_UPTIME {
            0
        } else {
            self.relaunches
        }
    }

    /// Relance après une erreur fatale : seulement si l'utilisateur n'avait pas demandé à
    /// quitter, si Windows ne ferme pas la session, si l'application tournait depuis un
    /// moment, et sans boucle infinie.
    pub fn should_relaunch(&self) -> bool {
        !self.exit_requested
            && !self.session_ended
            && self.uptime >= MIN_UPTIME
            && self.recent_relaunches() < MAX_RELAUNCHES
    }

    /// Compteur transmis au processus relancé.
    fn next_relaunch_count(&self) -> u32 {
        self.recent_relaunches() + 1
    }
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
pub fn relaunch(ctx: &EndContext) -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    std::process::Command::new(exe)
        .arg(RELAUNCH_ARG)
        .env(RELAUNCH_ENV, ctx.next_relaunch_count().to_string())
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

    const REASON: &str = "Enregistrement en cours · arrêtez la prise";

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
        let msg = match msg {
            WM_QUERYENDSESSION => SessionMsg::QueryEnd,
            WM_ENDSESSION => SessionMsg::End {
                ending: wparam != 0,
            },
            _ => return None,
        };
        match session_action(msg, take_in_progress()) {
            SessionAction::Refuse => {
                SAVE_REQUESTED.store(true, Ordering::Release);
                // Écran d'arrêt de Windows : « TracksPlus empêche l'arrêt » avec la
                // raison ; l'utilisateur peut annuler, la prise continue.
                set_reason(hwnd);
                SHUTDOWN_REFUSED.store(true, Ordering::Release);
                Some(0)
            }
            SessionAction::AllowAndSave => {
                SAVE_REQUESTED.store(true, Ordering::Release);
                None
            }
            SessionAction::FinishNow => {
                end_session_now();
                None
            }
            SessionAction::Nothing => None,
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

    fn ctx(secs: u64, relaunches: u32) -> EndContext {
        EndContext {
            uptime: Duration::from_secs(secs),
            relaunches,
            exit_requested: false,
            session_ended: false,
        }
    }

    #[test]
    fn relaunch_only_after_a_real_session_and_without_looping() {
        let minutes = 5 * 60;
        assert!(ctx(minutes, 0).should_relaunch());
        assert!(ctx(minutes, MAX_RELAUNCHES - 1).should_relaunch());
        // L'utilisateur avait demandé à quitter : jamais de relance.
        let quit = EndContext {
            exit_requested: true,
            ..ctx(minutes, 0)
        };
        assert!(!quit.should_relaunch());
        // Windows ferme la session : jamais de relance pendant l'arrêt.
        let ended = EndContext {
            session_ended: true,
            ..ctx(minutes, 0)
        };
        assert!(!ended.should_relaunch());
        // Erreur au démarrage : elle se reproduirait, pas de relance.
        assert!(!ctx(5, 0).should_relaunch());
        // Trop de relances rapprochées : on s'arrête.
        assert!(!ctx(minutes, MAX_RELAUNCHES).should_relaunch());
        assert_eq!(ctx(minutes, 2).next_relaunch_count(), 3);
        // Incidents espacés (une vraie session entre deux) : le compteur repart de zéro,
        // les relances ne s'épuisent pas sur un festival de plusieurs jours.
        let hours = 3 * 3600;
        assert!(ctx(hours, MAX_RELAUNCHES).should_relaunch());
        assert!(ctx(hours, 40).should_relaunch());
        assert_eq!(ctx(hours, MAX_RELAUNCHES).next_relaunch_count(), 1);
    }

    #[test]
    fn end_session_messages() {
        use SessionAction::*;
        // Prise en cours ou en finalisation : l'arrêt est refusé.
        assert_eq!(session_action(SessionMsg::QueryEnd, true), Refuse);
        // Sinon accepté, la session sauvegardée tout de suite.
        assert_eq!(session_action(SessionMsg::QueryEnd, false), AllowAndSave);
        for busy in [true, false] {
            // Arrêt confirmé (forcé malgré le refus, ou accepté) : fin d'urgence.
            assert_eq!(
                session_action(SessionMsg::End { ending: true }, busy),
                FinishNow
            );
            // Arrêt annulé par l'utilisateur : rien, la prise continue.
            assert_eq!(
                session_action(SessionMsg::End { ending: false }, busy),
                Nothing
            );
        }
    }

    #[test]
    fn pending_saves_are_written_once_session_first() {
        // (Seul test à utiliser les sauvegardes en attente.)
        let order = std::sync::Arc::new(Mutex::new(Vec::new()));
        let (a, b) = (order.clone(), order.clone());
        set_pending_board_save(Some(Box::new(move || a.lock().unwrap().push("soundboard"))));
        set_pending_session_save(Some(Box::new(move || b.lock().unwrap().push("session"))));
        write_pending_saves();
        write_pending_saves();
        assert_eq!(*order.lock().unwrap(), ["session", "soundboard"]);
        // Une sauvegarde qui échoue n'empêche pas la suivante.
        let c = order.clone();
        set_pending_session_save(Some(Box::new(|| panic!("disque"))));
        set_pending_board_save(Some(Box::new(move || c.lock().unwrap().push("encore"))));
        write_pending_saves();
        assert_eq!(order.lock().unwrap().last(), Some(&"encore"));
        // Retirée (plus rien à sauvegarder) : rien n'est écrit.
        let d = order.clone();
        set_pending_session_save(Some(Box::new(move || d.lock().unwrap().push("ancien"))));
        set_pending_session_save(None);
        write_pending_saves();
        assert_eq!(order.lock().unwrap().len(), 3);
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
