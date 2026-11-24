#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod audio;
mod export;
mod player;
mod project;
mod recorder;
mod render;
mod theme;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("TracksPlus")
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([900.0, 500.0]),
        ..Default::default()
    };
    eframe::run_native(
        "TracksPlus",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
}
