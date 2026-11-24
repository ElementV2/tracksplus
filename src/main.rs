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
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("TracksPlus")
            .with_inner_size([1440.0, 880.0])
            .with_min_inner_size([1100.0, 620.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "TracksPlus",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
}
