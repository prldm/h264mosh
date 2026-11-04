//! Export settings and their dialog.

use crate::tr;
use eframe::egui::{self, RichText};
use serde::{Deserialize, Serialize};

/// The kind of file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum Container {
    #[default]
    Mp4,
    Mkv,
    /// the moshed stream as a raw .264 file
    Raw,
}

/// How the video goes into an MP4 or MKV.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum VideoCoding {
    /// decoded and encoded again: the look is baked into an ordinary video
    #[default]
    Baked,
    /// the moshed stream as it is; players decode the mosh themselves
    Moshed,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum Quality {
    #[default]
    High,
    /// for further editing
    Lossless,
    Small,
}

impl Quality {
    /// libx264 preset and crf
    pub fn encoder(self) -> (&'static str, f32) {
        match self {
            Quality::High => ("slow", 16.0),
            Quality::Lossless => ("medium", 0.0),
            Quality::Small => ("medium", 23.0),
        }
    }

    fn name(self) -> String {
        match self {
            Quality::High => tr!("quality-high"),
            Quality::Lossless => tr!("quality-lossless"),
            Quality::Small => tr!("quality-small"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExportSettings {
    pub container: Container,
    pub video: VideoCoding,
    pub quality: Quality,
    /// the clips' audio and subtitles go along (MP4, MKV)
    pub keep_tracks: bool,
}

impl Default for ExportSettings {
    fn default() -> Self {
        Self { container: Container::Mp4, video: VideoCoding::Baked, quality: Quality::High, keep_tracks: true }
    }
}

impl ExportSettings {
    pub fn extension(&self) -> &'static str {
        match self.container {
            Container::Mp4 => "mp4",
            Container::Mkv => "mkv",
            Container::Raw => "264",
        }
    }

    /// The clips' other tracks go along.
    pub fn with_tracks(&self) -> bool {
        self.keep_tracks && self.container != Container::Raw
    }
}

/// The export dialog; `tracks`: the clips can have other tracks. Some(true): export
/// with `settings`, Some(false): cancelled.
pub fn dialog(ctx: &egui::Context, settings: &mut ExportSettings, tracks: bool) -> Option<bool> {
    let modal = egui::Modal::new(egui::Id::new("export-dialog")).show(ctx, |ui| {
        ui.set_width(420.0);
        ui.heading(tr!("export-title"));
        ui.add_space(6.0);
        ui.label(RichText::new(tr!("export-format")).strong());
        ui.horizontal(|ui| {
            ui.radio_value(&mut settings.container, Container::Mp4, tr!("export-mp4"));
            ui.radio_value(&mut settings.container, Container::Mkv, tr!("export-mkv"));
            ui.radio_value(&mut settings.container, Container::Raw, tr!("export-raw"));
        });
        let about = match settings.container {
            Container::Mp4 => tr!("export-mp4-about"),
            Container::Mkv => tr!("export-mkv-about"),
            Container::Raw => tr!("export-raw-about"),
        };
        ui.label(RichText::new(about).weak());
        if settings.container != Container::Raw {
            ui.add_space(8.0);
            ui.label(RichText::new(tr!("export-video")).strong());
            choice(ui, &mut settings.video, VideoCoding::Baked, tr!("export-baked"), tr!("export-baked-about"));
            if settings.video == VideoCoding::Baked {
                ui.indent("quality", |ui| {
                    egui::ComboBox::from_label(tr!("export-quality")).selected_text(settings.quality.name()).show_ui(ui, |ui| {
                        for q in [Quality::High, Quality::Lossless, Quality::Small] {
                            ui.selectable_value(&mut settings.quality, q, q.name());
                        }
                    });
                });
            }
            choice(ui, &mut settings.video, VideoCoding::Moshed, tr!("export-moshed"), tr!("export-moshed-about"));
            ui.add_space(8.0);
            ui.add_enabled_ui(tracks, |ui| {
                ui.checkbox(&mut settings.keep_tracks, tr!("export-keep-tracks")).on_disabled_hover_text(tr!("export-no-tracks"));
                ui.indent("tracks", |ui| ui.label(RichText::new(tr!("export-keep-tracks-about")).weak().small()));
            });
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button(tr!("export-choose-file")).clicked() {
                return Some(true);
            }
            ui.button(tr!("cancel-button")).clicked().then_some(false)
        })
        .inner
    });
    if modal.should_close() { Some(false) } else { modal.inner }
}

/// A video option with its explanation.
fn choice(ui: &mut egui::Ui, value: &mut VideoCoding, video: VideoCoding, name: String, about: String) {
    ui.radio_value(value, video, name);
    ui.indent(video_id(video), |ui| ui.label(RichText::new(about).weak().small()));
}

fn video_id(video: VideoCoding) -> &'static str {
    match video {
        VideoCoding::Baked => "baked",
        VideoCoding::Moshed => "moshed",
    }
}
