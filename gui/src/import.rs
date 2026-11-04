//! How new clips are encoded for moshing, and the dialog that asks for it when they are
//! opened.

use crate::project::Encoding;
use crate::tr;
use eframe::egui::{self, Align, Layout, RichText};
use h264mosh::pipeline::{BPyramid, Healing, Motion, Style};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImportSettings {
    pub style: Style,
    /// B-frames between the other frames
    pub bframes: u32,
    /// B-frames are references for other B-frames too (with 2 or more)
    pub b_pyramid: bool,
    /// reference frames
    pub refs: u32,
}

impl Default for ImportSettings {
    fn default() -> Self {
        Self { style: Style::default(), bframes: 0, b_pyramid: false, refs: 1 }
    }
}

impl ImportSettings {
    pub fn encoding(&self) -> Encoding {
        let pyramid = if self.b_pyramid && self.bframes >= 2 { BPyramid::Normal } else { BPyramid::None };
        Encoding::for_import(self.style, self.refs, self.bframes, pyramid)
    }
}

/// The dialog shown before clips are encoded. Some(true): encode with `settings`,
/// Some(false): cancelled. `raw`: the clip is a raw stream, which is used as it is if it
/// can be edited.
pub fn dialog(ctx: &egui::Context, settings: &mut ImportSettings, raw: bool) -> Option<bool> {
    let modal = egui::Modal::new(egui::Id::new("import-dialog")).show(ctx, |ui| {
        ui.set_width(460.0);
        ui.heading(tr!("import-title"));
        ui.label(RichText::new(tr!("import-intro")).weak());
        if raw {
            ui.label(RichText::new(tr!("import-raw-note")).weak());
        }
        ui.add_space(8.0);
        let s = settings;

        title(ui, tr!("import-motion"));
        ui.horizontal(|ui| {
            for (motion, name) in [
                (Motion::Flowing, tr!("motion-flowing")),
                (Motion::Balanced, tr!("motion-balanced")),
                (Motion::Detailed, tr!("motion-detailed")),
            ] {
                ui.selectable_value(&mut s.style.motion, motion, name);
            }
        });
        about(
            ui,
            match s.style.motion {
                Motion::Flowing => tr!("motion-flowing-about"),
                Motion::Balanced => tr!("motion-balanced-about"),
                Motion::Detailed => tr!("motion-detailed-about"),
            },
        );

        title(ui, tr!("import-healing"));
        ui.horizontal(|ui| {
            for (healing, name) in
                [(Healing::Slow, tr!("healing-slow")), (Healing::Normal, tr!("healing-normal")), (Healing::Fast, tr!("healing-fast"))]
            {
                ui.selectable_value(&mut s.style.healing, healing, name);
            }
        });
        about(
            ui,
            match s.style.healing {
                Healing::Slow => tr!("healing-slow-about"),
                Healing::Normal => tr!("healing-normal-about"),
                Healing::Fast => tr!("healing-fast-about"),
            },
        );

        title(ui, tr!("import-bframes"));
        ui.horizontal(|ui| {
            ui.selectable_value(&mut s.bframes, 0, tr!("bframes-none"));
            for n in 1..=3 {
                ui.selectable_value(&mut s.bframes, n, n.to_string());
            }
            ui.add_space(8.0);
            ui.add_enabled_ui(s.bframes >= 2, |ui| {
                ui.checkbox(&mut s.b_pyramid, tr!("bframes-pyramid")).on_hover_text(tr!("bframes-pyramid-tooltip"));
            });
        });
        about(ui, if s.bframes == 0 { tr!("bframes-none-about") } else { tr!("bframes-about") });

        title(ui, tr!("import-refs"));
        ui.horizontal(|ui| {
            for n in [1, 2, 4] {
                ui.selectable_value(&mut s.refs, n, n.to_string());
            }
        });
        about(ui, if s.refs == 1 { tr!("refs-one-about") } else { tr!("refs-more-about") });

        ui.separator();
        ui.label(RichText::new(format!("libx264 {}", s.encoding().describe())).weak().small().monospace());
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.button(tr!("import-encode")).clicked() {
                return Some(true);
            }
            let cancel = ui.button(tr!("cancel-button")).clicked();
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button(tr!("import-recommended")).on_hover_text(tr!("import-recommended-tooltip")).clicked() {
                    *s = ImportSettings::default();
                }
            });
            cancel.then_some(false)
        })
        .inner
    });
    if modal.should_close() { Some(false) } else { modal.inner }
}

fn title(ui: &mut egui::Ui, text: String) {
    ui.label(RichText::new(text).strong());
}

/// What to expect from the option chosen above.
fn about(ui: &mut egui::Ui, text: String) {
    ui.label(RichText::new(text).weak());
    ui.add_space(6.0);
}
