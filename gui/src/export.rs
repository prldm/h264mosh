//! Export settings and their dialog.

use crate::tr;
use eframe::egui::{self, RichText};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum ExportFormat {
    /// decoded and encoded again: the look is baked into an ordinary video
    #[default]
    Baked,
    /// the moshed stream in an mp4 as it is; players decode the mosh themselves
    Copy,
    /// the moshed stream as a raw .264 file
    Raw,
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ExportSettings {
    pub format: ExportFormat,
    pub quality: Quality,
}

impl ExportSettings {
    pub fn extension(&self) -> &'static str {
        match self.format {
            ExportFormat::Raw => "264",
            _ => "mp4",
        }
    }
}

/// The export dialog. Some(true): export with `settings`, Some(false): cancelled.
pub fn dialog(ctx: &egui::Context, settings: &mut ExportSettings) -> Option<bool> {
    let modal = egui::Modal::new(egui::Id::new("export-dialog")).show(ctx, |ui| {
        ui.set_min_width(380.0);
        ui.heading(tr!("export-title"));
        ui.add_space(6.0);
        choice(ui, &mut settings.format, ExportFormat::Baked, tr!("export-baked"), tr!("export-baked-about"));
        if settings.format == ExportFormat::Baked {
            ui.indent("quality", |ui| {
                egui::ComboBox::from_label(tr!("export-quality")).selected_text(settings.quality.name()).show_ui(ui, |ui| {
                    for q in [Quality::High, Quality::Lossless, Quality::Small] {
                        ui.selectable_value(&mut settings.quality, q, q.name());
                    }
                });
            });
        }
        choice(ui, &mut settings.format, ExportFormat::Copy, tr!("export-copy"), tr!("export-copy-about"));
        choice(ui, &mut settings.format, ExportFormat::Raw, tr!("export-raw"), tr!("export-raw-about"));
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

/// A format option with its explanation.
fn choice(ui: &mut egui::Ui, value: &mut ExportFormat, format: ExportFormat, name: String, about: String) {
    ui.radio_value(value, format, name);
    ui.indent(format_id(format), |ui| ui.label(RichText::new(about).weak().small()));
}

fn format_id(format: ExportFormat) -> &'static str {
    match format {
        ExportFormat::Baked => "baked",
        ExportFormat::Copy => "copy",
        ExportFormat::Raw => "raw",
    }
}
