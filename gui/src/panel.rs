//! The effect panel: the effect lanes, the parameters of the active one, and adding
//! effects to the selected frames.

use crate::edit::{CardId, Cards, EffectId, affects};
use crate::project::FrameInfo;
use crate::tr;
use eframe::egui::{self, Color32, RichText, Sense, Vec2};
use h264mosh::effects::MbEffect;
use std::collections::HashSet;

/// What the panel asks for.
pub enum Action {
    /// a new effect on the selected frames
    Add(MbEffect),
    Select(Option<EffectId>),
    Params(EffectId, MbEffect),
    Enabled(EffectId, bool),
    /// give the effect to the selected frames, or take it away
    Apply(EffectId, bool),
    Delete(EffectId),
    /// move a lane up (-1) or down (+1); lanes apply from top to bottom
    Reorder(EffectId, isize),
    Seed(u64),
}

const PALETTE: [Color32; 8] = [
    Color32::from_rgb(86, 180, 233),
    Color32::from_rgb(0, 170, 120),
    Color32::from_rgb(204, 121, 167),
    Color32::from_rgb(150, 120, 220),
    Color32::from_rgb(60, 190, 190),
    Color32::from_rgb(240, 120, 150),
    Color32::from_rgb(150, 200, 60),
    Color32::from_rgb(120, 150, 255),
];

/// Color of the effect lane at `index`.
pub fn effect_color(index: usize) -> Color32 {
    PALETTE[index % PALETTE.len()]
}

/// The effects on offer, with their starting parameters.
fn kinds() -> [MbEffect; 6] {
    [
        MbEffect::MvScale(2.0, 2.0),
        MbEffect::MvAdd(16, 0),
        MbEffect::MvJitter(16),
        MbEffect::MvdScale(2.0, 2.0),
        MbEffect::KillIntra,
        MbEffect::ZeroResidual,
    ]
}

pub fn effect_name(e: &MbEffect) -> String {
    match e {
        MbEffect::MvScale(..) => tr!("effect-mv-scale"),
        MbEffect::MvAdd(..) => tr!("effect-mv-shift"),
        MbEffect::MvJitter(_) => tr!("effect-mv-jitter"),
        MbEffect::MvdScale(..) => tr!("effect-mvd-scale"),
        MbEffect::KillIntra => tr!("effect-kill-intra"),
        MbEffect::ZeroResidual => tr!("effect-zero-residual"),
        _ => tr!("effect-debug"),
    }
}

fn effect_about(e: &MbEffect) -> String {
    match e {
        MbEffect::MvScale(..) => tr!("effect-mv-scale-about"),
        MbEffect::MvAdd(..) => tr!("effect-mv-shift-about"),
        MbEffect::MvJitter(_) => tr!("effect-mv-jitter-about"),
        MbEffect::MvdScale(..) => tr!("effect-mvd-scale-about"),
        MbEffect::KillIntra => tr!("effect-kill-intra-about"),
        MbEffect::ZeroResidual => tr!("effect-zero-residual-about"),
        _ => tr!("effect-debug"),
    }
}

/// The parameters in a few characters.
pub fn effect_summary(e: &MbEffect) -> String {
    let px = |q: i32| q as f64 / 4.0;
    match *e {
        MbEffect::MvScale(x, y) | MbEffect::MvdScale(x, y) => tr!("summary-scale", x = x as f64, y = y as f64),
        MbEffect::MvAdd(x, y) => tr!("summary-shift", x = px(x), y = px(y)),
        MbEffect::MvJitter(n) => tr!("summary-jitter", px = px(n)),
        _ => String::new(),
    }
}

/// Name and parameters, for lanes and lists.
pub fn effect_label(e: &MbEffect) -> String {
    let summary = effect_summary(e);
    if summary.is_empty() { effect_name(e) } else { tr!("effect-label", name = effect_name(e), summary = summary) }
}

pub fn show(
    ui: &mut egui::Ui,
    cards: &Cards,
    frames: &[FrameInfo],
    selection: &HashSet<CardId>,
    active: Option<EffectId>,
    cavlc: bool,
) -> Vec<Action> {
    let mut actions = Vec::new();
    ui.heading(tr!("effects-title"));
    ui.add_space(4.0);

    let can_add = cavlc && !selection.is_empty();
    let why = if !cavlc { tr!("effects-need-cavlc") } else { tr!("effects-select-first") };
    ui.add_enabled_ui(can_add, |ui| {
        ui.menu_button(tr!("effect-add"), |ui| {
            for kind in kinds() {
                if ui.button(effect_name(&kind)).on_hover_text(effect_about(&kind)).clicked() {
                    actions.push(Action::Add(kind));
                    ui.close();
                }
            }
        })
        .response
        .on_disabled_hover_text(why.clone());
    });
    if !cavlc {
        ui.label(RichText::new(tr!("effects-need-cavlc")).color(ui.visuals().warn_fg_color).small());
    }

    // lanes, applied from top to bottom
    ui.add_space(6.0);
    let lanes = cards.effects.len();
    for (k, fx) in cards.effects.iter().enumerate() {
        ui.horizontal(|ui| {
            let (swatch, _) = ui.allocate_exact_size(Vec2::new(10.0, 14.0), Sense::hover());
            ui.painter().rect_filled(swatch, 2.0, effect_color(k));
            let mut enabled = fx.enabled;
            if ui.checkbox(&mut enabled, "").on_hover_text(tr!("effect-enabled-tooltip")).changed() {
                actions.push(Action::Enabled(fx.id, enabled));
            }
            if ui.selectable_label(active == Some(fx.id), effect_label(&fx.effect)).clicked() {
                actions.push(Action::Select(if active == Some(fx.id) { None } else { Some(fx.id) }));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add_enabled(k + 1 < lanes, egui::Button::new("⏷").small()).on_hover_text(tr!("effect-down")).clicked() {
                    actions.push(Action::Reorder(fx.id, 1));
                }
                if ui.add_enabled(k > 0, egui::Button::new("⏶").small()).on_hover_text(tr!("effect-up")).clicked() {
                    actions.push(Action::Reorder(fx.id, -1));
                }
            });
        });
    }
    if lanes == 0 {
        ui.label(RichText::new(tr!("effects-none")).weak());
    }

    let Some(fx) = active.and_then(|id| cards.effect(id)) else { return actions };
    ui.separator();
    ui.label(RichText::new(effect_name(&fx.effect)).strong());
    ui.label(RichText::new(effect_about(&fx.effect)).weak().small());
    ui.add_space(4.0);
    let mut effect = fx.effect.clone();
    params(ui, &mut effect, cards.seed, &mut actions);
    if effect != fx.effect {
        actions.push(Action::Params(fx.id, effect));
    }

    let used: Vec<_> = cards.cards.iter().filter(|c| c.effects.contains(&fx.id)).collect();
    let idle = used.iter().filter(|c| !affects(&fx.effect, frames[c.src].kind)).count();
    ui.add_space(4.0);
    ui.label(tr!("effect-used", count = used.len()));
    if idle > 0 {
        ui.label(RichText::new(tr!("effect-idle", count = idle)).weak().small());
    }
    ui.horizontal_wrapped(|ui| {
        let any = !selection.is_empty();
        if ui.add_enabled(any, egui::Button::new(tr!("effect-apply"))).on_hover_text(tr!("effect-apply-tooltip")).clicked() {
            actions.push(Action::Apply(fx.id, true));
        }
        if ui.add_enabled(any, egui::Button::new(tr!("effect-remove"))).on_hover_text(tr!("effect-remove-tooltip")).clicked() {
            actions.push(Action::Apply(fx.id, false));
        }
        if ui.button(tr!("effect-delete")).clicked() {
            actions.push(Action::Delete(fx.id));
        }
    });
    actions
}

/// Sliders for the parameters of `effect`.
fn params(ui: &mut egui::Ui, effect: &mut MbEffect, seed: u64, actions: &mut Vec<Action>) {
    let factor = |ui: &mut egui::Ui, v: &mut f32, label: String| {
        ui.add(egui::Slider::new(v, -8.0..=8.0).step_by(0.05).text(label));
    };
    // quarter samples, shown in pixels
    let pixels = |ui: &mut egui::Ui, q: &mut i32, range: std::ops::RangeInclusive<f32>, label: String| {
        let mut px = *q as f32 / 4.0;
        if ui.add(egui::Slider::new(&mut px, range).step_by(0.25).text(label)).changed() {
            *q = (px * 4.0).round() as i32;
        }
    };
    match effect {
        MbEffect::MvScale(x, y) | MbEffect::MvdScale(x, y) => {
            factor(ui, x, tr!("param-horizontal"));
            factor(ui, y, tr!("param-vertical"));
        }
        MbEffect::MvAdd(x, y) => {
            pixels(ui, x, -64.0..=64.0, tr!("param-horizontal-px"));
            pixels(ui, y, -64.0..=64.0, tr!("param-vertical-px"));
        }
        MbEffect::MvJitter(n) => {
            pixels(ui, n, 0.0..=64.0, tr!("param-amount-px"));
            ui.horizontal(|ui| {
                let mut s = seed;
                if ui.add(egui::DragValue::new(&mut s).prefix(tr!("param-pattern"))).changed() {
                    actions.push(Action::Seed(s));
                }
                if ui.button(tr!("param-new-pattern")).clicked() {
                    actions.push(Action::Seed(new_seed(seed)));
                }
            });
        }
        _ => {}
    }
}

/// A different seed, derived from the clock.
fn new_seed(old: u64) -> u64 {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1);
    (t ^ old.rotate_left(17)) % 1_000_000
}
