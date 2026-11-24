#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod audio;
mod export;
mod midi;
mod player;
mod project;
mod recorder;
mod render;
mod sampler;
mod soundboard;
mod ui;

fn main() -> eframe::Result {
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
