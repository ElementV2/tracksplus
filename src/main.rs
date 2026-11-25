#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod audio;
mod decode;
mod encode;
mod export;
#[cfg(test)]
mod fuzz_tests;

// Tests : toute allocation à l'intérieur de `assert_no_alloc(|| …)` fait échouer le test.
#[cfg(all(test, debug_assertions))]
#[global_allocator]
static ALLOC: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;
mod import;
mod isolate;
mod midi;
mod player;
mod project;
mod recorder;
mod recovery;
mod render;
mod sampler;
mod sessions;
mod shutdown;
mod soundboard;
mod ui;
mod update;

/// Une seule instance à la fois : deux TracksPlus se disputeraient la carte son (ASIO
/// n'accepte qu'une application) et les mêmes réglages. Le verrou est tenu tant que le
/// processus vit ; le système le libère tout seul, même après un crash.
/// Retourne le fichier verrouillé (à garder ouvert), ou quitte si l'application tourne déjà.
fn single_instance() -> Option<std::fs::File> {
    let dir = dirs::config_dir()?.join("TracksPlus");
    std::fs::create_dir_all(&dir).ok()?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(dir.join("instance.lock"))
        .ok()?;
    match file.try_lock() {
        Ok(()) => Some(file),
        Err(std::fs::TryLockError::WouldBlock) => {
            rfd::MessageDialog::new()
                .set_title("TracksPlus")
                .set_description(
                    "TracksPlus est déjà ouvert.\n\nLa fenêtre déjà lancée va être affichée.",
                )
                .set_level(rfd::MessageLevel::Info)
                .set_buttons(rfd::MessageButtons::Ok)
                .show();
            focus_existing_window();
            std::process::exit(0);
        }
        // Verrouillage non géré par le disque : on ne bloque jamais le démarrage.
        Err(_) => None,
    }
}

/// Ramène au premier plan la fenêtre de l'instance déjà lancée.
#[cfg(windows)]
fn focus_existing_window() {
    use windows_sys::Win32::Foundation::{HWND, LPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextW, IsIconic, IsWindowVisible, SW_RESTORE, SetForegroundWindow,
        ShowWindow,
    };
    unsafe extern "system" fn visit(h: HWND, _: LPARAM) -> i32 {
        let mut buf = [0u16; 512];
        // SAFETY : `h` vient d'EnumWindows, `buf` a la taille annoncée.
        let n = unsafe { GetWindowTextW(h, buf.as_mut_ptr(), buf.len() as i32) };
        if n <= 0 || unsafe { IsWindowVisible(h) } == 0 {
            return 1;
        }
        let title = String::from_utf16_lossy(&buf[..n as usize]);
        // Titre de la fenêtre : « TracksPlus » ou « <session> — TracksPlus ».
        if title == "TracksPlus" || title.ends_with("— TracksPlus") {
            unsafe {
                if IsIconic(h) != 0 {
                    ShowWindow(h, SW_RESTORE);
                }
                SetForegroundWindow(h);
            }
            return 0;
        }
        1
    }
    // SAFETY : rappel `extern "system"` conforme à WNDENUMPROC.
    unsafe {
        EnumWindows(Some(visit), 0);
    }
}

#[cfg(not(windows))]
fn focus_existing_window() {}

/// Au-delà de cette taille, crash.log est renommé en crash.old.log (l'ancien est remplacé).
const CRASH_LOG_MAX: u64 = 1024 * 1024;

/// Une même erreur (message + emplacement) n'est notée qu'une fois par seconde : une
/// erreur répétée à chaque image ne remplit pas le disque.
fn crash_log_allowed(
    recent: &mut std::collections::HashMap<String, std::time::Instant>,
    key: &str,
    now: std::time::Instant,
) -> bool {
    let window = std::time::Duration::from_secs(1);
    recent.retain(|_, t| now.saturating_duration_since(*t) < window);
    if recent.contains_key(key) {
        return false;
    }
    recent.insert(key.to_string(), now);
    true
}

/// Erreurs récemment notées (filtre de `crash_log_allowed`).
static CRASH_RECENT: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
> = std::sync::LazyLock::new(Default::default);

/// Ajoute une entrée datée à crash.log (dossier de configuration).
fn append_crash_log(text: &str) {
    let Some(dir) = dirs::config_dir().map(|d| d.join("TracksPlus")) else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("crash.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > CRASH_LOG_MAX) {
        let _ = std::fs::rename(&path, dir.join("crash.old.log"));
    }
    let line = format!(
        "[{}] {text}\n\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    );
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Note une erreur interne qui n'est pas une panique (carte graphique…) dans crash.log.
/// `key` identifie l'erreur : une même erreur n'est notée qu'une fois par seconde.
pub fn log_error(key: &str, text: &str) {
    let allowed = match CRASH_RECENT.lock() {
        Ok(mut l) => crash_log_allowed(&mut l, key, std::time::Instant::now()),
        Err(_) => false,
    };
    if allowed {
        append_crash_log(text);
    }
}

/// Écrit les erreurs internes dans un journal (dossier de configuration) pour diagnostic.
fn install_crash_log() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let key = format!(
            "{}|{}",
            info.location().map(|l| l.to_string()).unwrap_or_default(),
            info.payload_as_str().unwrap_or("")
        );
        let allowed = match CRASH_RECENT.lock() {
            Ok(mut l) => crash_log_allowed(&mut l, &key, std::time::Instant::now()),
            Err(_) => false,
        };
        if allowed {
            let bt = std::backtrace::Backtrace::force_capture();
            append_crash_log(&format!("{info}\n{bt}"));
        }
        default(info);
    }));
}

/// Délais d'attente des prises encore en écriture à la fin du processus : court après une
/// fermeture normale (tout est déjà fini, sauf disque bloqué), plus long après une erreur
/// (les prises passent avant la relance).
const END_WRITERS_NORMAL: std::time::Duration = std::time::Duration::from_secs(2);
const END_WRITERS_CRASH: std::time::Duration = std::time::Duration::from_secs(10);
/// Copies de sécurité après une erreur : déjà terminées avec les prises (leurs threads
/// d'écriture sont attendus ci-dessus) ; un second disque mort ne retarde pas la relance.
const END_BACKUPS_CRASH: std::time::Duration = std::time::Duration::from_secs(2);

fn main() -> eframe::Result {
    // Processus fils de décodage (voir `isolate`) : ni fenêtre, ni journal, ni verrou.
    if let Some(code) = isolate::worker_main() {
        std::process::exit(code);
    }
    isolate::enable();
    install_crash_log();
    // Gardé jusqu'à la fermeture de l'application.
    let instance = single_instance();
    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_title("TracksPlus")
        .with_inner_size([1440.0, 880.0])
        .with_min_inner_size([1280.0, 720.0])
        .with_drag_and_drop(true);
    // Icône de la fenêtre et de la barre des tâches.
    if let Ok(icon) =
        eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon/tracksplus.png"))
    {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    let started = std::time::Instant::now();
    // Une erreur fatale hors des garde-fous de l'interface (rendu, winit, eframe) remonte
    // jusqu'ici : l'application a déjà été détruite, ce qui a arrêté et finalisé les prises
    // (voir `Drop for App`). On la rattrape pour finir proprement et relancer.
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        eframe::run_native(
            "TracksPlus",
            options,
            Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
        )
    }));
    // Prise encore en écriture (fermeture interrompue, disque lent) : ses tampons sont
    // écrits et ses fichiers fermés avant que le processus disparaisse.
    let (writers, backups) = if res.is_ok() {
        (END_WRITERS_NORMAL, END_WRITERS_NORMAL)
    } else {
        (END_WRITERS_CRASH, END_BACKUPS_CRASH)
    };
    recorder::emergency_finish(writers);
    recorder::wait_backups(backups, backups);
    match res {
        Ok(r) => r,
        Err(_) => {
            // En plein spectacle, l'application revient d'elle-même dans sa session (les
            // prises interrompues sont réintégrées au démarrage).
            let end = shutdown::EndContext::now(started.elapsed());
            if end.should_relaunch() {
                // Verrou d'instance unique libéré avant : sinon la relance se croirait en double.
                drop(instance);
                if let Err(e) = shutdown::relaunch(&end) {
                    log_error("relaunch", &format!("Relance automatique impossible : {e}"));
                }
            }
            std::process::exit(101);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn crash_log_deduplicates_within_a_second() {
        let mut recent = std::collections::HashMap::new();
        let t = Instant::now();
        let at = |ms| t + Duration::from_millis(ms);
        assert!(crash_log_allowed(&mut recent, "a.rs:1|boom", at(0)));
        assert!(!crash_log_allowed(&mut recent, "a.rs:1|boom", at(500)));
        // Une autre erreur passe, sans relancer la première.
        assert!(crash_log_allowed(&mut recent, "b.rs:2|autre", at(600)));
        assert!(!crash_log_allowed(&mut recent, "a.rs:1|boom", at(700)));
        assert!(crash_log_allowed(&mut recent, "a.rs:1|boom", at(1100)));
        assert!(!crash_log_allowed(&mut recent, "b.rs:2|autre", at(1200)));
    }
}
