//! The time bar under the preview: the last render from start to end, with a time
//! ruler, its keyframes and the playhead. Clicking or dragging moves the playhead;
//! hovering tells the time and the frame there.

use crate::timeline::{I_COLOR, kind_name, timestamp};
use crate::tr;
use eframe::egui::{self, FontId, Rect, Sense, Shape, Stroke, pos2, vec2};

const HEIGHT: f32 = 28.0;
const TRACK: f32 = 12.0;
/// room at the ends for the playhead's head
const INSET: f32 = 6.0;
/// ruler labels are at least this far apart
const LABEL_SPACING: f32 = 64.0;

/// What the bar shows.
pub struct Bar<'a> {
    /// output frames of the render
    pub frames: usize,
    pub fps: f64,
    pub playhead: usize,
    /// source frame and frame type of output frame k
    pub source: &'a dyn Fn(usize) -> (usize, char),
}

/// Show the bar; returns the frame clicked or dragged to.
pub fn show(ui: &mut egui::Ui, bar: Bar) -> Option<usize> {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), HEIGHT), Sense::click_and_drag());
    if bar.frames == 0 {
        return None;
    }
    let track = Rect::from_min_size(rect.min + vec2(INSET, 2.0), vec2((rect.width() - 2.0 * INSET).max(1.0), TRACK));
    let per_frame = track.width() / bar.frames as f32;
    let x_of = |k: usize| track.left() + (k as f32 + 0.5) * per_frame;
    let frame_at = |x: f32| (((x - track.left()) / per_frame).floor().max(0.0) as usize).min(bar.frames - 1);
    let visuals = ui.visuals();
    let painter = ui.painter_at(rect);
    painter.rect_filled(track, 2.0, visuals.extreme_bg_color);

    // keyframes
    for k in (0..bar.frames).filter(|&k| (bar.source)(k).1 == 'I') {
        painter.vline(x_of(k), track.y_range(), Stroke::new(1.0, I_COLOR));
    }
    // ruler: the shortest step whose labels do not crowd
    let seconds = bar.frames as f64 / bar.fps;
    let step = [0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 900.0, 1800.0, 3600.0]
        .into_iter()
        .find(|&s| (s / seconds) as f32 * track.width() >= LABEL_SPACING)
        .unwrap_or(7200.0);
    let font = FontId::proportional(10.0);
    for t in (0..).map(|n| n as f64 * step).take_while(|&t| t <= seconds) {
        let x = track.left() + (t / seconds) as f32 * track.width();
        painter.vline(x, track.bottom()..=track.bottom() + 3.0, Stroke::new(1.0, visuals.weak_text_color()));
        let label = timestamp(t);
        // whole seconds without hundredths
        let label = if step >= 1.0 { label.trim_end_matches(".00").to_string() } else { label };
        let galley = painter.layout_no_wrap(label, font.clone(), visuals.weak_text_color());
        // centred under the tick, but inside the bar
        let left = (x - galley.size().x / 2.0).clamp(rect.left(), rect.right() - galley.size().x);
        painter.galley(pos2(left, track.bottom() + 3.0), galley, visuals.weak_text_color());
    }
    // the playhead
    let x = x_of(bar.playhead.min(bar.frames - 1));
    let color = visuals.strong_text_color();
    painter.vline(x, track.y_range().expand(1.0), Stroke::new(2.0, color));
    let tip = pos2(x, track.top());
    painter.add(Shape::convex_polygon(vec![tip, tip + vec2(-5.0, -3.0), tip + vec2(5.0, -3.0)], color, Stroke::NONE));

    let seek = if response.clicked() || response.dragged() { response.interact_pointer_pos().map(|p| frame_at(p.x)) } else { None };
    if let Some(p) = response.hover_pos()
        && !response.dragged()
    {
        let k = frame_at(p.x);
        painter.vline(x_of(k), track.y_range(), Stroke::new(1.0, visuals.text_color()));
        let (src, kind) = (bar.source)(k);
        let time = timestamp(k as f64 / bar.fps);
        response.on_hover_text_at_pointer(tr!("timebar-tooltip", frame = k, time = time, source = src, kind = kind_name(kind)));
    }
    seek
}
