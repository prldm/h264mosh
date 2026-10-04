//! h264mosh GUI: frame-level datamosh editing on top of the h264mosh core.

// release builds are windowed apps; debug builds keep a console for logs
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod i18n;

use eframe::egui;

// the core allocates many small, short-lived buffers on all threads
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(tr!("app-title"))
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([640.0, 400.0]),
        ..Default::default()
    };
    eframe::run_native("h264mosh", options, Box::new(|cc| Ok(Box::new(app::App::new(cc)))))
}
