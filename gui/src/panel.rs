//! The effect panel: the effect lanes, the parameters of the active one, and adding
//! effects to the selected frames.

use crate::edit::{CardId, Cards, EffectId, affects};
use crate::project::FrameInfo;
use crate::tr;
use eframe::egui::{self, Color32, RichText, Sense, Vec2};
use h264mosh::effects::{Mask, MbEffect};
use std::collections::HashSet;
use std::ops::RangeInclusive;

/// What the panel asks for.
pub enum Action {
    /// a new effect on the selected frames
    Add(MbEffect),
    Select(Option<EffectId>),
    Params(EffectId, MbEffect),
    /// limit the effect to an area of the picture, or not
    Mask(EffectId, Option<Mask>),
    Enabled(EffectId, bool),
    /// give the effect to the selected frames, or take it away
    Apply(EffectId, bool),
    Delete(EffectId),
    /// move a lane up (-1) or down (+1); lanes apply from top to bottom
    Reorder(EffectId, isize),
    Seed(u64),
}

/// What the panel shows besides the edit.
pub struct Context<'a> {
    pub frames: &'a [FrameInfo],
    /// source frames in display order
    pub display: &'a [usize],
    pub selection: &'a HashSet<CardId>,
    pub active: Option<EffectId>,
    /// CAVLC entropy coding (macroblock effects need it)
    pub cavlc: bool,
    /// reference frames of the stream
    pub refs: u32,
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

/// The effects on offer, by group, with their starting parameters.
fn groups() -> [(String, Vec<MbEffect>); 5] {
    [
        (
            tr!("group-motion"),
            vec![
                MbEffect::MvScale(2.0, 2.0),
                MbEffect::MvAdd(16, 0),
                MbEffect::MvJitter(16),
                MbEffect::MvdScale(2.0, 2.0),
                MbEffect::MotionTransplant(0, 1.0),
                MbEffect::RefShift(1),
            ],
        ),
        (tr!("group-blocks"), vec![MbEffect::KillIntra, MbEffect::ZeroResidual, MbEffect::Transplant(0, 0.5)]),
        (tr!("group-intra"), vec![MbEffect::IntraMode(0), MbEffect::IntraRotate(2), MbEffect::IntraRandom(0.5), MbEffect::IntraCascade(3)]),
        (tr!("group-residual"), vec![MbEffect::QpAdd(12), MbEffect::QpSet(51), MbEffect::Coeffs(0.25, 0.0), MbEffect::DcShift(1, 0, 0)]),
        (tr!("group-picture"), vec![MbEffect::Deblock(6, 6), MbEffect::Weight(1.02, 0), MbEffect::Tint(1, -1)]),
    ]
}

pub fn effect_name(e: &MbEffect) -> String {
    match e {
        MbEffect::MvScale(..) => tr!("effect-mv-scale"),
        MbEffect::MvAdd(..) => tr!("effect-mv-shift"),
        MbEffect::MvJitter(_) => tr!("effect-mv-jitter"),
        MbEffect::MvdScale(..) => tr!("effect-mvd-scale"),
        MbEffect::RefShift(_) => tr!("effect-ref-shift"),
        MbEffect::KillIntra => tr!("effect-kill-intra"),
        MbEffect::ZeroResidual => tr!("effect-zero-residual"),
        MbEffect::Transplant(..) => tr!("effect-transplant"),
        MbEffect::MotionTransplant(..) => tr!("effect-motion-transplant"),
        MbEffect::IntraMode(_) => tr!("effect-intra-mode"),
        MbEffect::IntraRotate(_) => tr!("effect-intra-rotate"),
        MbEffect::IntraRandom(_) => tr!("effect-intra-random"),
        MbEffect::IntraCascade(_) => tr!("effect-intra-cascade"),
        MbEffect::QpAdd(_) => tr!("effect-qp-add"),
        MbEffect::QpSet(_) => tr!("effect-qp-set"),
        MbEffect::Coeffs(..) => tr!("effect-coeffs"),
        MbEffect::DcShift(..) => tr!("effect-dc-shift"),
        MbEffect::Deblock(..) => tr!("effect-deblock"),
        MbEffect::Weight(..) => tr!("effect-weight"),
        MbEffect::Tint(..) => tr!("effect-tint"),
        MbEffect::ExplicitSkips | MbEffect::SplitTo8x8 => tr!("effect-debug"),
    }
}

fn effect_about(e: &MbEffect) -> String {
    match e {
        MbEffect::MvScale(..) => tr!("effect-mv-scale-about"),
        MbEffect::MvAdd(..) => tr!("effect-mv-shift-about"),
        MbEffect::MvJitter(_) => tr!("effect-mv-jitter-about"),
        MbEffect::MvdScale(..) => tr!("effect-mvd-scale-about"),
        MbEffect::RefShift(_) => tr!("effect-ref-shift-about"),
        MbEffect::KillIntra => tr!("effect-kill-intra-about"),
        MbEffect::ZeroResidual => tr!("effect-zero-residual-about"),
        MbEffect::Transplant(..) => tr!("effect-transplant-about"),
        MbEffect::MotionTransplant(..) => tr!("effect-motion-transplant-about"),
        MbEffect::IntraMode(_) => tr!("effect-intra-mode-about"),
        MbEffect::IntraRotate(_) => tr!("effect-intra-rotate-about"),
        MbEffect::IntraRandom(_) => tr!("effect-intra-random-about"),
        MbEffect::IntraCascade(_) => tr!("effect-intra-cascade-about"),
        MbEffect::QpAdd(_) => tr!("effect-qp-add-about"),
        MbEffect::QpSet(_) => tr!("effect-qp-set-about"),
        MbEffect::Coeffs(..) => tr!("effect-coeffs-about"),
        MbEffect::DcShift(..) => tr!("effect-dc-shift-about"),
        MbEffect::Deblock(..) => tr!("effect-deblock-about"),
        MbEffect::Weight(..) => tr!("effect-weight-about"),
        MbEffect::Tint(..) => tr!("effect-tint-about"),
        MbEffect::ExplicitSkips | MbEffect::SplitTo8x8 => tr!("effect-debug"),
    }
}

/// Name of an intra prediction direction (Intra4x4PredMode numbering).
fn direction_name(d: u8) -> String {
    match d {
        0 => tr!("direction-vertical"),
        1 => tr!("direction-horizontal"),
        2 => tr!("direction-dc"),
        3 => tr!("direction-down-left"),
        4 => tr!("direction-down-right"),
        5 => tr!("direction-vertical-right"),
        6 => tr!("direction-horizontal-down"),
        7 => tr!("direction-vertical-left"),
        _ => tr!("direction-horizontal-up"),
    }
}

/// The parameters in a few characters.
/// Display position of source frame `src` (`display`: source frames in display order),
/// the frame number shown to the user.
pub fn shown_at(display: &[usize], src: usize) -> usize {
    display.iter().position(|&s| s == src).unwrap_or(src)
}

/// The parameters in short; frames are named by display position (`display`).
pub fn effect_summary(e: &MbEffect, display: &[usize]) -> String {
    let px = |q: i32| q as f64 / 4.0;
    match *e {
        MbEffect::MvScale(x, y) | MbEffect::MvdScale(x, y) => tr!("summary-scale", x = x as f64, y = y as f64),
        MbEffect::MvAdd(x, y) => tr!("summary-shift", x = px(x), y = px(y)),
        MbEffect::MvJitter(n) => tr!("summary-jitter", px = px(n)),
        MbEffect::RefShift(n) => tr!("summary-frames", n = format!("{n:+}")),
        MbEffect::Transplant(frame, amount) | MbEffect::MotionTransplant(frame, amount) => {
            tr!("summary-transplant", frame = shown_at(display, frame), percent = amount as f64 * 100.0)
        }
        MbEffect::IntraMode(d) => direction_name(d),
        MbEffect::IntraRotate(n) => tr!("summary-degrees", degrees = n as f64 * 22.5),
        MbEffect::IntraRandom(p) => tr!("summary-percent", percent = p as f64 * 100.0),
        MbEffect::IntraCascade(n) => tr!("summary-number", n = n),
        MbEffect::QpAdd(n) => tr!("summary-signed", n = format!("{n:+}")),
        MbEffect::QpSet(n) => tr!("summary-number", n = n),
        MbEffect::Coeffs(from, factor) => tr!("summary-coeffs", factor = factor as f64, percent = from as f64 * 100.0),
        MbEffect::DcShift(y, cb, cr) => tr!("summary-three", a = y, b = cb, c = cr),
        MbEffect::Deblock(a, b) | MbEffect::Tint(a, b) => tr!("summary-two", a = a, b = b),
        MbEffect::Weight(gain, offset) => tr!("summary-weight", gain = gain as f64, offset = offset),
        _ => String::new(),
    }
}

/// Name and parameters, for lanes and lists.
pub fn effect_label(e: &MbEffect, display: &[usize]) -> String {
    let summary = effect_summary(e, display);
    if summary.is_empty() { effect_name(e) } else { tr!("effect-label", name = effect_name(e), summary = summary) }
}

pub fn show(ui: &mut egui::Ui, cards: &Cards, c: &Context) -> Vec<Action> {
    let mut actions = Vec::new();
    ui.heading(tr!("effects-title"));
    ui.add_space(4.0);

    ui.add_enabled_ui(!c.selection.is_empty(), |ui| {
        ui.menu_button(tr!("effect-add"), |ui| {
            for (group, kinds) in groups() {
                ui.menu_button(group, |ui| {
                    for kind in kinds {
                        let usable = c.cavlc || !kind.needs_macroblocks();
                        let button = ui
                            .add_enabled(usable, egui::Button::new(effect_name(&kind)))
                            .on_hover_text(effect_about(&kind))
                            .on_disabled_hover_text(tr!("effects-need-cavlc"));
                        if button.clicked() {
                            actions.push(Action::Add(kind));
                            ui.close();
                        }
                    }
                });
            }
        })
        .response
        .on_disabled_hover_text(tr!("effects-select-first"));
    });
    if !c.cavlc {
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
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add_enabled(k + 1 < lanes, egui::Button::new("⏷").small()).on_hover_text(tr!("effect-down")).clicked() {
                    actions.push(Action::Reorder(fx.id, 1));
                }
                if ui.add_enabled(k > 0, egui::Button::new("⏶").small()).on_hover_text(tr!("effect-up")).clicked() {
                    actions.push(Action::Reorder(fx.id, -1));
                }
                // the name takes the room that is left, shortened if needed
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    let label = effect_label(&fx.effect, c.display);
                    let name = ui.add(egui::Button::selectable(c.active == Some(fx.id), label.clone()).truncate());
                    if name.on_hover_text(label).clicked() {
                        actions.push(Action::Select(if c.active == Some(fx.id) { None } else { Some(fx.id) }));
                    }
                });
            });
        });
    }
    if lanes == 0 {
        ui.label(RichText::new(tr!("effects-none")).weak());
    }

    let Some(fx) = c.active.and_then(|id| cards.effect(id)) else { return actions };
    ui.separator();
    ui.label(RichText::new(effect_name(&fx.effect)).strong());
    ui.label(RichText::new(effect_about(&fx.effect)).weak().small());
    if matches!(fx.effect, MbEffect::RefShift(_)) && c.refs < 2 {
        ui.label(RichText::new(tr!("effect-ref-shift-one-ref")).color(ui.visuals().warn_fg_color).small());
    }
    ui.add_space(4.0);
    let mut effect = fx.effect.clone();
    params(ui, &mut effect, cards.seed, c.display, &mut actions);
    if effect != fx.effect {
        actions.push(Action::Params(fx.id, effect));
    }
    if fx.effect.needs_macroblocks() {
        ui.add_space(4.0);
        area(ui, fx.id, &fx.mask, &mut actions);
    }

    let used: Vec<_> = cards.cards.iter().filter(|card| card.effects.contains(&fx.id)).collect();
    let idle = used.iter().filter(|card| !affects(&fx.effect, c.frames[card.src].kind)).count();
    ui.add_space(4.0);
    ui.label(tr!("effect-used", count = used.len()));
    if idle > 0 {
        ui.label(RichText::new(tr!("effect-idle", count = idle)).weak().small());
    }
    ui.horizontal_wrapped(|ui| {
        let any = !c.selection.is_empty();
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
/// Sliders for the parameters of `effect`; frames are chosen by display position.
fn params(ui: &mut egui::Ui, effect: &mut MbEffect, seed: u64, display: &[usize], actions: &mut Vec<Action>) {
    let factor = |ui: &mut egui::Ui, v: &mut f32, label: String| {
        ui.add(egui::Slider::new(v, -8.0..=8.0).step_by(0.05).text(label));
    };
    // quarter samples, shown in pixels
    let pixels = |ui: &mut egui::Ui, q: &mut i32, range: RangeInclusive<f32>, label: String| {
        let mut px = *q as f32 / 4.0;
        if ui.add(egui::Slider::new(&mut px, range).step_by(0.25).text(label)).changed() {
            *q = (px * 4.0).round() as i32;
        }
    };
    let int = |ui: &mut egui::Ui, v: &mut i32, range: RangeInclusive<i32>, label: String| {
        ui.add(egui::Slider::new(v, range).text(label));
    };
    // a fraction, shown in percent
    let percent = |ui: &mut egui::Ui, v: &mut f32, label: String| {
        let mut p = *v * 100.0;
        if ui.add(egui::Slider::new(&mut p, 0.0..=100.0).step_by(1.0).suffix("%").text(label)).changed() {
            *v = p / 100.0;
        }
    };
    // a source frame, chosen by its display position
    let frame = |ui: &mut egui::Ui, src: &mut usize| {
        let mut k = shown_at(display, *src);
        let last = display.len().saturating_sub(1);
        if ui.add(egui::DragValue::new(&mut k).range(0..=last).prefix(tr!("param-from-frame"))).changed() {
            *src = display.get(k).copied().unwrap_or(k);
        }
    };
    let pattern = |ui: &mut egui::Ui, actions: &mut Vec<Action>| {
        ui.horizontal(|ui| {
            let mut s = seed;
            if ui.add(egui::DragValue::new(&mut s).prefix(tr!("param-pattern"))).changed() {
                actions.push(Action::Seed(s));
            }
            if ui.button(tr!("param-new-pattern")).clicked() {
                actions.push(Action::Seed(new_seed(seed)));
            }
        });
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
            pattern(ui, actions);
        }
        MbEffect::RefShift(n) => int(ui, n, -15..=15, tr!("param-frames-back")),
        MbEffect::Transplant(src, amount) => {
            frame(ui, src);
            percent(ui, amount, tr!("param-amount"));
            if *amount < 1.0 {
                pattern(ui, actions);
            }
        }
        MbEffect::MotionTransplant(src, mix) => {
            frame(ui, src);
            percent(ui, mix, tr!("param-mix"));
        }
        MbEffect::IntraMode(d) => {
            egui::ComboBox::from_label(tr!("param-direction")).selected_text(direction_name(*d)).show_ui(ui, |ui| {
                for k in 0..=8 {
                    ui.selectable_value(d, k, direction_name(k));
                }
            });
        }
        MbEffect::IntraRotate(n) => {
            ui.add(
                egui::Slider::new(n, -7..=7).text(tr!("param-turn")).custom_formatter(|v, _| tr!("summary-degrees", degrees = v * 22.5)),
            );
        }
        MbEffect::IntraRandom(p) => {
            percent(ui, p, tr!("param-amount"));
            pattern(ui, actions);
        }
        MbEffect::IntraCascade(n) => {
            ui.add(egui::Slider::new(n, 0..=7).text(tr!("param-code")));
        }
        MbEffect::QpAdd(n) => int(ui, n, -30..=30, tr!("param-quantizer-offset")),
        MbEffect::QpSet(n) => int(ui, n, 0..=51, tr!("param-quantizer")),
        MbEffect::Coeffs(from, f) => {
            percent(ui, from, tr!("param-from-frequency"));
            factor(ui, f, tr!("param-factor"));
        }
        // these build up from block to block and frame to frame: small steps
        MbEffect::DcShift(y, cb, cr) => {
            int(ui, y, -16..=16, tr!("param-brightness"));
            int(ui, cb, -16..=16, tr!("param-blue"));
            int(ui, cr, -16..=16, tr!("param-red"));
        }
        MbEffect::Deblock(a, b) => {
            int(ui, a, -6..=6, tr!("param-deblock-alpha"));
            int(ui, b, -6..=6, tr!("param-deblock-beta"));
        }
        MbEffect::Weight(gain, offset) => {
            ui.add(egui::Slider::new(gain, 0.5..=1.5).step_by(0.01).text(tr!("param-gain")));
            int(ui, offset, -16..=16, tr!("param-offset"));
        }
        MbEffect::Tint(cb, cr) => {
            int(ui, cb, -16..=16, tr!("param-blue"));
            int(ui, cr, -16..=16, tr!("param-red"));
        }
        _ => {}
    }
}

/// The part of the picture an effect applies to: all of it, or a rectangle (or all
/// but a rectangle).
fn area(ui: &mut egui::Ui, id: EffectId, mask: &Option<Mask>, actions: &mut Vec<Action>) {
    let mut on = mask.is_some();
    let mut m = mask.clone().filter(|m| !m.rects.is_empty()).unwrap_or_else(|| Mask::rect(0.25, 0.25, 0.5, 0.5));
    ui.checkbox(&mut on, tr!("area-limit")).on_hover_text(tr!("area-limit-tooltip"));
    if on {
        let [x, y, w, h] = &mut m.rects[0];
        let edge = |ui: &mut egui::Ui, v: &mut f32, min: f32, label: String| {
            let mut p = *v * 100.0;
            if ui.add(egui::Slider::new(&mut p, min..=100.0).step_by(1.0).suffix("%").text(label)).changed() {
                *v = p / 100.0;
            }
        };
        edge(ui, x, 0.0, tr!("area-left"));
        edge(ui, y, 0.0, tr!("area-top"));
        edge(ui, w, 1.0, tr!("area-width"));
        edge(ui, h, 1.0, tr!("area-height"));
        ui.checkbox(&mut m.invert, tr!("area-invert"));
    }
    let new = on.then_some(m);
    if new != *mask {
        actions.push(Action::Mask(id, new));
    }
}

/// A different seed, derived from the clock.
fn new_seed(old: u64) -> u64 {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1);
    (t ^ old.rotate_left(17)) % 1_000_000
}
