#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod audio;
mod export;
mod midi;
mod player;
mod project;
mod recorder;
mod recovery;
mod render;
mod sampler;
mod soundboard;
mod ui;

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

/// Écrit les erreurs internes dans un journal (dossier de configuration) pour diagnostic.
fn install_crash_log() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(dir) = dirs::config_dir().map(|d| d.join("TracksPlus")) {
            let _ = std::fs::create_dir_all(&dir);
            let bt = std::backtrace::Backtrace::force_capture();
            let line = format!(
                "[{}] {info}\n{bt}\n\n",
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
            );
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("crash.log"))
            {
                let _ = f.write_all(line.as_bytes());
            }
        }
        default(info);
    }));
}

fn main() -> eframe::Result {
    install_crash_log();
    // Gardé jusqu'à la fermeture de l'application.
    let _instance = single_instance();
    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_title("TracksPlus")
        .with_inner_size([1440.0, 880.0])
        .with_min_inner_size([1100.0, 620.0])
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
    eframe::run_native(
        "TracksPlus",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
}
