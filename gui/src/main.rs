//! h264mosh GUI: frame-level datamosh editing on top of the h264mosh core.

// release builds are windowed apps; debug builds keep a console for logs
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod audio;
mod edit;
mod export;
mod history;
mod i18n;
mod import;
mod jobs;
mod panel;
mod preview;
mod project;
mod settings;
mod timebar;
mod timeline;

use eframe::egui;

// the core allocates many small, short-lived buffers on all threads
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// The app icon at 256x256, RGBA (rendered from assets/icon.svg by build.rs).
const ICON: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/icon-256.rgba"));

fn main() -> eframe::Result {
    let icon = egui::IconData { rgba: ICON.to_vec(), width: 256, height: 256 };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(tr!("app-title"))
            .with_icon(icon)
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([640.0, 400.0]),
        ..Default::default()
    };
    eframe::run_native("h264mosh", options, Box::new(|cc| Ok(Box::new(app::App::new(cc)))))
}
