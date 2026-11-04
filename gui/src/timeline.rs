//! The frame strip: one card per timeline card, zoomable from the whole timeline in
//! view (several cards to a pixel are drawn as columns) up to thumbnails, with a lane
//! per effect underneath. Click, Shift-click and Ctrl-click select, dragging moves the
//! selection, the wheel over a card's copy badge changes its count, Ctrl+wheel zooms.
//! The edges of an effect block can be dragged.
//!
//! Edits are not made here: they come back as [`Event`]s, so the app can record them
//! for undo.

use crate::edit::{CardId, Cards, EffectId, affects};
use crate::history::Gesture;
use crate::panel::{effect_color, effect_label};
use crate::project::FrameInfo;
use crate::tr;
use eframe::egui::{self, Align2, Color32, CursorIcon, FontId, Painter, Rect, Sense, Shape, Stroke, StrokeKind, Vec2, pos2, vec2};
use std::collections::HashSet;
use std::ops::Range;

/// narrower cards are plain I/P ticks without thumbnails or labels
const DETAIL_ZOOM: f32 = 24.0;
/// cards this many pixels wide and wider have a divider between them
const DIVIDER_PIXELS: f32 = 3.0;
/// height of the "not rendered" band above a card
const BAND: f32 = 5.0;
const MARGIN: f32 = 4.0;
const LABEL: f32 = 14.0;
const SCROLLBAR: f32 = 8.0;
/// height of an effect lane, and the distance between lanes
const LANE: f32 = 14.0;
const LANE_STEP: f32 = 16.0;
/// how close to a block edge the pointer grabs it
const EDGE_GRAB: f32 = 6.0;
/// wheel distance for one copy more or less
const WHEEL_STEP: f32 = 40.0;

pub const I_COLOR: Color32 = Color32::from_rgb(236, 142, 58);
const P_COLOR: Color32 = Color32::from_rgb(92, 104, 128);
const B_COLOR: Color32 = Color32::from_rgb(150, 100, 190);
const PENDING: Color32 = Color32::from_rgb(242, 200, 40);

/// What the user did in the strip.
pub enum Event {
    /// show this card in the preview
    Seek(CardId),
    Move {
        ids: HashSet<CardId>,
        before: usize,
    },
    Copies {
        ids: HashSet<CardId>,
        delta: i64,
        gesture: Gesture,
    },
    Restore(CardId),
    SelectEffect(Option<EffectId>),
    /// give the effect to these cards (`on`) or take it away
    EffectSpan {
        id: EffectId,
        cards: HashSet<CardId>,
        on: bool,
    },
}

/// What the strip shows.
pub struct Strip<'a> {
    pub cards: &'a Cards,
    pub frames: &'a [FrameInfo],
    /// source frames in display order
    pub display: &'a [usize],
    pub thumbs: &'a [Option<egui::TextureHandle>],
    /// picture height / width
    pub aspect: f32,
    pub playhead: Option<CardId>,
    pub active_effect: Option<EffectId>,
    /// the wheel changes the copies of the selection (B held)
    pub bloom: bool,
    /// frames per second, for the times in tooltips
    pub fps: f64,
}

/// Dragging an edge of an effect block: the effect, its lane, the block (first and
/// last card) and which edge.
#[derive(Clone, Copy)]
struct EdgeDrag {
    id: EffectId,
    lane: usize,
    block: (usize, usize),
    right: bool,
}

pub struct TimelineView {
    /// points per card, divider included (clamped between the whole timeline in view
    /// and the widest cards that fit)
    pub zoom: f32,
    /// horizontal scroll in points
    scroll: f32,
    pub selection: HashSet<CardId>,
    anchor: Option<CardId>,
    /// cards being dragged
    drag: Option<HashSet<CardId>>,
    edge_drag: Option<EdgeDrag>,
    scrollbar_drag: bool,
    wheel: f32,
    /// geometry of the last frame
    last: Option<Layout>,
    min_zoom: f32,
    max_zoom: f32,
}

impl Default for TimelineView {
    fn default() -> Self {
        Self {
            zoom: f32::MAX,
            scroll: 0.0,
            selection: HashSet::new(),
            anchor: None,
            drag: None,
            edge_drag: None,
            scrollbar_drag: false,
            wheel: 0.0,
            last: None,
            min_zoom: 1.0,
            max_zoom: 200.0,
        }
    }
}

/// Positions of the cards and lanes for the strip size and zoom.
#[derive(Clone, Copy)]
struct Layout {
    rect: Rect,
    /// points per card, divider included
    zoom: f32,
    gap: f32,
    scroll: f32,
    card_h: f32,
    lanes: usize,
    pixels_per_point: f32,
}

impl Layout {
    fn new(rect: Rect, zoom: f32, scroll: f32, lanes: usize, pixels_per_point: f32) -> Self {
        let gap = gap(zoom, pixels_per_point);
        let labels = if zoom - gap >= DETAIL_ZOOM { LABEL } else { 0.0 };
        // cards fill the strip height; narrower ones show the middle of their thumbnail
        let card_h = (Self::room(rect, lanes) + LABEL - labels).max(4.0);
        Self { rect, zoom, gap, scroll, card_h, lanes, pixels_per_point }
    }

    /// card height when labels are shown
    fn room(rect: Rect, lanes: usize) -> f32 {
        rect.height() - 2.0 * MARGIN - BAND - 2.0 - LABEL - lanes_height(lanes) - SCROLLBAR
    }

    /// widest cards whose thumbnails fit the strip height
    fn max_zoom(rect: Rect, aspect: f32, lanes: usize) -> f32 {
        let width = Self::room(rect, lanes) / aspect;
        (width / 0.92).min(width + 8.0).max(1.0)
    }

    fn slot(&self) -> f32 {
        self.zoom
    }

    fn detailed(&self) -> bool {
        self.zoom - self.gap >= DETAIL_ZOOM
    }

    /// several cards to a pixel, drawn as columns
    fn dense(&self) -> bool {
        self.zoom * self.pixels_per_point < 1.0
    }

    /// `x` on the pixel grid
    fn snap(&self, x: f32) -> f32 {
        (x * self.pixels_per_point).round() / self.pixels_per_point
    }

    /// where the slot of card `i` begins
    fn slot_left(&self, i: usize) -> f32 {
        self.rect.left() + i as f32 * self.zoom - self.scroll
    }

    /// the card on whole pixels, the divider after it
    fn card_rect(&self, i: usize) -> Rect {
        let x = self.slot_left(i);
        let (left, right) = (self.snap(x), self.snap(x + self.zoom) - self.gap);
        let top = self.rect.top() + MARGIN + BAND + 2.0;
        Rect::from_min_max(pos2(left, top), pos2(right.max(left), top + self.card_h))
    }

    fn band_rect(&self, card: Rect) -> Rect {
        Rect::from_min_size(pos2(card.left(), self.rect.top() + MARGIN), vec2(card.width(), BAND))
    }

    fn lanes_top(&self) -> f32 {
        self.rect.bottom() - SCROLLBAR - lanes_height(self.lanes)
    }

    /// lane under the y coordinate
    fn lane_at(&self, y: f32) -> Option<usize> {
        let k = ((y - self.lanes_top()) / LANE_STEP).floor();
        (k >= 0.0 && (k as usize) < self.lanes).then_some(k as usize)
    }

    /// the part of lane `k` under card `i`
    fn lane_rect(&self, k: usize, i: usize) -> Rect {
        let card = self.card_rect(i);
        Rect::from_min_size(pos2(card.left(), self.lanes_top() + k as f32 * LANE_STEP), vec2(card.width(), LANE))
    }

    /// card under the x coordinate (gaps belong to the card on their right)
    fn index_at(&self, x: f32) -> usize {
        ((x - self.rect.left() + self.scroll) / self.slot()).floor().max(0.0) as usize
    }

    /// insertion point (0..=len) nearest to the x coordinate
    fn gap_at(&self, x: f32, len: usize) -> usize {
        (((x - self.rect.left() + self.scroll) / self.slot()).round().max(0.0) as usize).min(len)
    }

    fn badge_rect(&self, card: Rect) -> Rect {
        let w = 26.0f32.min(card.width());
        Rect::from_min_size(card.right_top() - vec2(w, 0.0), vec2(w, 14.0))
    }

    fn restore_rect(&self, card: Rect) -> Rect {
        Rect::from_center_size(card.center(), Vec2::splat(20.0f32.min(card.width())))
    }

    fn scrollbar(&self) -> Rect {
        Rect::from_min_max(pos2(self.rect.left(), self.rect.bottom() - SCROLLBAR), self.rect.right_bottom())
    }
}

fn lanes_height(lanes: usize) -> f32 {
    if lanes == 0 { 0.0 } else { lanes as f32 * LANE_STEP + 2.0 }
}

/// The block of cards around `i` that have effect `id`.
fn block(cards: &Cards, id: EffectId, i: usize) -> Option<(usize, usize)> {
    let has = |j: usize| cards.cards[j].effects.contains(&id);
    if !has(i) {
        return None;
    }
    let (mut a, mut b) = (i, i);
    while a > 0 && has(a - 1) {
        a -= 1;
    }
    while b + 1 < cards.len() && has(b + 1) {
        b += 1;
    }
    Some((a, b))
}

impl TimelineView {
    pub fn select_all(&mut self, cards: &Cards) {
        self.selection = cards.cards.iter().map(|c| c.id).collect();
    }

    pub fn zoom_range(&self) -> std::ops::RangeInclusive<f32> {
        self.min_zoom..=self.max_zoom
    }

    /// Scroll the card at `index` into view.
    pub fn reveal(&mut self, index: usize) {
        let Some(l) = self.last else { return };
        let x = index as f32 * l.slot();
        if x < self.scroll || x + l.slot() > self.scroll + l.rect.width() {
            self.scroll = (x + l.slot() / 2.0 - l.rect.width() / 2.0).max(0.0);
        }
    }

    /// Where card `index` was drawn last.
    #[cfg(test)]
    pub fn card_rect(&self, index: usize) -> Rect {
        self.last.expect("shown").card_rect(index)
    }

    /// Where lane `k` was drawn last under card `index`.
    #[cfg(test)]
    pub fn lane_rect(&self, k: usize, index: usize) -> Rect {
        self.last.expect("shown").lane_rect(k, index)
    }

    /// The edge of an effect block near `p`.
    fn edge_at(&self, l: &Layout, cards: &Cards, p: egui::Pos2) -> Option<EdgeDrag> {
        let k = l.lane_at(p.y)?;
        let id = cards.effects[k].id;
        let i = l.index_at(p.x).min(cards.len().checked_sub(1)?);
        let near = |x: f32| (p.x - x).abs() <= EDGE_GRAB;
        // the block under the pointer, or the ones just left and right of it
        for j in [Some(i), i.checked_sub(1), (i + 1 < cards.len()).then_some(i + 1)].into_iter().flatten() {
            if let Some((a, b)) = block(cards, id, j) {
                if near(l.card_rect(b).right()) {
                    return Some(EdgeDrag { id, lane: k, block: (a, b), right: true });
                }
                if near(l.card_rect(a).left()) {
                    return Some(EdgeDrag { id, lane: k, block: (a, b), right: false });
                }
            }
        }
        None
    }

    pub fn show(&mut self, ui: &mut egui::Ui, s: Strip) -> Vec<Event> {
        let cards = s.cards;
        let (rect, response) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
        let ppp = ui.ctx().pixels_per_point();
        let lanes = cards.effects.len();
        let len = cards.len();
        let max_zoom = Layout::max_zoom(rect, s.aspect, lanes);
        // zoomed out all the way, the whole timeline is in view
        let min_zoom = (rect.width() / len.max(1) as f32).min(max_zoom);
        (self.min_zoom, self.max_zoom) = (min_zoom, max_zoom);
        self.zoom = self.zoom.clamp(min_zoom, max_zoom);
        let mut events = Vec::new();

        // the wheel zooms around the pointer, scrolls, or changes copies
        if let Some(p) = response.hover_pos() {
            let (zoom, delta) = ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta));
            let l = Layout::new(rect, self.zoom, self.scroll, lanes, ppp);
            let i = l.index_at(p.x);
            let on_badge = i < len && !cards.cards[i].deleted && l.detailed() && l.badge_rect(l.card_rect(i)).contains(p);
            let copies = if s.bloom && !self.selection.is_empty() {
                Some((self.selection.clone(), Gesture::SelectionCopies))
            } else {
                on_badge.then(|| (HashSet::from([cards.cards[i].id]), Gesture::Copies(cards.cards[i].id)))
            };
            if zoom != 1.0 {
                let old = self.zoom;
                self.zoom = (self.zoom * zoom).clamp(min_zoom, max_zoom);
                let anchor = p.x - rect.left();
                self.scroll = (anchor + self.scroll) * self.zoom / old - anchor;
            } else if delta != Vec2::ZERO {
                if let Some((ids, gesture)) = copies {
                    self.wheel += delta.y + delta.x;
                    let steps = (self.wheel / WHEEL_STEP).trunc();
                    if steps != 0.0 {
                        self.wheel -= steps * WHEEL_STEP;
                        events.push(Event::Copies { ids, delta: steps as i64, gesture });
                    }
                } else {
                    self.scroll -= delta.x + delta.y;
                }
            }
            ui.input_mut(|i| i.smooth_scroll_delta = Vec2::ZERO);
        }

        let content = len as f32 * self.zoom;
        let max_scroll = (content - rect.width()).max(0.0);
        self.scroll = self.scroll.clamp(0.0, max_scroll);
        let l = Layout::new(rect, self.zoom, self.scroll, lanes, ppp);
        self.last = Some(l);
        let pointer = response.interact_pointer_pos().or(response.hover_pos());
        let in_lanes = |p: egui::Pos2| l.lane_at(p.y).is_some();

        // scrollbar
        let bar = l.scrollbar();
        let thumb = (max_scroll > 0.0).then(|| {
            let w = (rect.width() / content * bar.width()).max(24.0);
            let x = bar.left() + self.scroll / max_scroll * (bar.width() - w);
            Rect::from_min_size(pos2(x, bar.top()), vec2(w, bar.height()))
        });
        if response.drag_started() && thumb.is_some() && pointer.is_some_and(|p| bar.contains(p)) {
            self.scrollbar_drag = true;
        }
        if self.scrollbar_drag {
            if let Some(t) = thumb {
                self.scroll += response.drag_delta().x * max_scroll / (bar.width() - t.width()).max(1.0);
            }
            if response.drag_stopped() {
                self.scrollbar_drag = false;
            }
        }

        // clicks: effect blocks, restore buttons, selection
        if response.clicked()
            && let Some(p) = pointer
            && !bar.contains(p)
        {
            let i = l.index_at(p.x);
            if let Some(k) = l.lane_at(p.y) {
                let id = cards.effects[k].id;
                let hit = i < len && cards.cards[i].effects.contains(&id);
                events.push(Event::SelectEffect(hit.then_some(id)));
            } else if i < len {
                let card = &cards.cards[i];
                if card.deleted && l.detailed() && l.restore_rect(l.card_rect(i)).contains(p) {
                    events.push(Event::Restore(card.id));
                } else {
                    self.click(cards, i, ui.input(|i| i.modifiers));
                    events.push(Event::Seek(card.id));
                }
            } else {
                self.selection.clear();
            }
        }

        // dragging an effect edge, or cards; what is dragged is decided where the button
        // went down, the pointer has moved on when a drag starts
        if pointer.is_some_and(|p| self.edge_at(&l, cards, p).is_some()) || self.edge_drag.is_some() {
            ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
        }
        if response.drag_started()
            && !self.scrollbar_drag
            && let Some(p) = ui.input(|i| i.pointer.press_origin()).or(pointer)
        {
            if in_lanes(p) {
                self.edge_drag = self.edge_at(&l, cards, p);
            } else {
                let i = l.index_at(p.x);
                if i < len {
                    let id = cards.cards[i].id;
                    if !self.selection.contains(&id) {
                        self.selection = HashSet::from([id]);
                        self.anchor = Some(id);
                    }
                    self.drag = Some(self.selection.clone());
                }
            }
        }
        // the block an edge drag leads to
        let mut resized = None;
        if let (Some(e), Some(p)) = (self.edge_drag, pointer) {
            let t = l.index_at(p.x).min(len - 1);
            let (a, b) = e.block;
            let new = if e.right { (a, t.max(a)) } else { (t.min(b), b) };
            resized = Some((e.lane, new));
            if response.drag_stopped() {
                let outside = |r: (usize, usize), other: (usize, usize)| -> HashSet<CardId> {
                    (r.0..=r.1).filter(|j| !(other.0..=other.1).contains(j)).map(|j| cards.cards[j].id).collect()
                };
                let (added, removed) = (outside(new, (a, b)), outside((a, b), new));
                if !added.is_empty() {
                    events.push(Event::EffectSpan { id: e.id, cards: added, on: true });
                }
                if !removed.is_empty() {
                    events.push(Event::EffectSpan { id: e.id, cards: removed, on: false });
                }
                self.edge_drag = None;
                resized = None;
            }
        }
        let mut insert_at = None;
        if let Some(ids) = &self.drag {
            ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
            if let Some(p) = pointer {
                insert_at = Some(l.gap_at(p.x, len));
                // scroll while dragging near the edges
                let edge = 40.0;
                if p.x < rect.left() + edge || p.x > rect.right() - edge {
                    self.scroll += if p.x < rect.left() + edge { -8.0 } else { 8.0 };
                    ui.ctx().request_repaint();
                }
            }
            if response.drag_stopped() {
                if let Some(at) = insert_at {
                    events.push(Event::Move { ids: ids.clone(), before: at });
                }
                self.drag = None;
                insert_at = None;
            }
        }

        // paint
        let visuals = ui.visuals();
        let hovered =
            pointer.filter(|p| rect.contains(*p) && !bar.contains(*p) && !in_lanes(*p)).map(|p| l.index_at(p.x)).filter(|&i| i < len);
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, visuals.extreme_bg_color);
        let visible = l.index_at(rect.left())..(l.index_at(rect.right()) + 1).min(len);
        if l.dense() {
            self.paint_columns(&painter, visuals, &l, &s, visible.clone());
        } else {
            for i in visible.clone() {
                let card = &cards.cards[i];
                let style = CardStyle {
                    kind: s.frames[card.src].kind,
                    thumb: s.thumbs.get(card.src).and_then(Option::as_ref),
                    shown: cards.shown(i),
                    selected: self.selection.contains(&card.id),
                    pending: cards.is_pending(i),
                    playhead: s.playhead == Some(card.id),
                    hovered: hovered == Some(i),
                    pointer,
                };
                paint_card(&painter, visuals, &l, card, l.card_rect(i), &style);
            }
            for (k, fx) in cards.effects.iter().enumerate() {
                let color = effect_color(k);
                for i in visible.clone() {
                    let card = &cards.cards[i];
                    if !card.effects.contains(&fx.id) {
                        continue;
                    }
                    let mut r = l.lane_rect(k, i);
                    if i + 1 < len && cards.cards[i + 1].effects.contains(&fx.id) {
                        r.max.x += l.gap;
                    }
                    // dimmed where the effect changes nothing (frame type, bypassed)
                    let working = fx.enabled && affects(&fx.effect, s.frames[card.src].kind);
                    painter.rect_filled(r, 0.0, if working { color } else { color.gamma_multiply(0.35) });
                }
            }
        }
        if let Some(k) = cards.effects.iter().position(|e| Some(e.id) == s.active_effect) {
            // outline each visible block once, from its first visible card
            let id = cards.effects[k].id;
            let has = |i: usize| cards.cards[i].effects.contains(&id);
            for i in visible.clone().filter(|&i| has(i) && (i == visible.start || !has(i - 1))) {
                let (a, b) = block(cards, id, i).expect("the card has the effect");
                let r = l.lane_rect(k, a).union(l.lane_rect(k, b));
                painter.rect_stroke(r, 2.0, Stroke::new(1.5, visuals.strong_text_color()), StrokeKind::Outside);
            }
        }
        if let Some((lane, (a, b))) = resized {
            let r = l.lane_rect(lane, a).union(l.lane_rect(lane, b));
            painter.rect_stroke(r, 2.0, Stroke::new(2.0, visuals.selection.stroke.color), StrokeKind::Outside);
        }
        if let Some(at) = insert_at {
            let x = l.card_rect(at).left() - l.gap / 2.0;
            painter.vline(x, rect.y_range().shrink(MARGIN), Stroke::new(3.0, visuals.selection.stroke.color));
        }
        if let Some(t) = thumb {
            painter.rect_filled(bar, 0.0, visuals.faint_bg_color);
            painter.rect_filled(t.shrink(1.0), 3.0, visuals.widgets.inactive.bg_fill);
        }

        if self.drag.is_none() && self.edge_drag.is_none() {
            let lane = pointer.and_then(|p| Some((l.lane_at(p.y)?, l.index_at(p.x))));
            if let Some((k, _)) = lane.filter(|&(k, i)| i < len && cards.cards[i].effects.contains(&cards.effects[k].id)) {
                response.on_hover_text_at_pointer(effect_label(&cards.effects[k].effect, s.display));
            } else if let Some(i) = hovered {
                response.on_hover_text_at_pointer(tooltip(cards, i, s.frames, s.fps));
            }
        }
        events
    }

    /// Several cards to a pixel: a column per pixel, in the colour of the frames shown
    /// in it (a keyframe wins), with the marks of any card in it.
    fn paint_columns(&self, painter: &Painter, visuals: &egui::Visuals, l: &Layout, s: &Strip, visible: Range<usize>) {
        let cards = s.cards;
        let px = 1.0 / l.pixels_per_point;
        let card = l.card_rect(visible.start);
        let (top, bottom) = (card.top(), card.bottom());
        let selected = visuals.selection.stroke.color;
        let mut x = l.snap(l.slot_left(visible.start).max(l.rect.left()));
        while x < l.rect.right() {
            let a = l.index_at(x).max(visible.start);
            if a >= visible.end {
                break;
            }
            let column = a..(l.index_at(x + px - 0.001) + 1).min(visible.end);
            let (mut p, mut b, mut keyframe) = (0, 0, false);
            let (mut sel, mut pending, mut playhead, mut removed) = (false, false, false, false);
            for i in column.clone() {
                let card = &cards.cards[i];
                if !card.deleted {
                    match s.frames[card.src].kind {
                        'I' => keyframe = true,
                        'B' => b += 1,
                        _ => p += 1,
                    }
                }
                sel |= self.selection.contains(&card.id);
                pending |= cards.is_pending(i);
                playhead |= s.playhead == Some(card.id);
                removed |= !card.removed_before.is_empty();
            }
            let color = if keyframe {
                I_COLOR
            } else if p + b == 0 {
                visuals.weak_text_color().gamma_multiply(0.4)
            } else {
                P_COLOR.lerp_to_gamma(B_COLOR, b as f32 / (p + b) as f32)
            };
            let r = Rect::from_min_max(pos2(x, top), pos2(x + px, bottom));
            painter.rect_filled(r, 0.0, color);
            if pending {
                painter.rect_filled(l.band_rect(r), 0.0, PENDING);
            }
            if removed {
                painter.rect_filled(r, 0.0, visuals.error_fg_color);
            }
            if sel {
                painter.rect_filled(Rect::from_min_max(pos2(x, top - 1.0), pos2(x + px, top + 2.0)), 0.0, selected);
                painter.rect_filled(Rect::from_min_max(pos2(x, bottom - 2.0), pos2(x + px, bottom + 1.0)), 0.0, selected);
            }
            if playhead {
                let tip = pos2(x + px / 2.0, top - 1.0);
                let head = vec![tip, tip + vec2(-5.0, -6.0), tip + vec2(5.0, -6.0)];
                painter.add(Shape::convex_polygon(head, visuals.strong_text_color(), Stroke::NONE));
            }
            for (k, fx) in cards.effects.iter().enumerate() {
                let with: Vec<usize> = column.clone().filter(|&i| cards.cards[i].effects.contains(&fx.id)).collect();
                if with.is_empty() {
                    continue;
                }
                let working = fx.enabled && with.iter().any(|&i| affects(&fx.effect, s.frames[cards.cards[i].src].kind));
                let lane = l.lane_rect(k, a);
                let color = effect_color(k);
                let r = Rect::from_min_max(pos2(x, lane.top()), pos2(x + px, lane.bottom()));
                painter.rect_filled(r, 0.0, if working { color } else { color.gamma_multiply(0.35) });
            }
            x += px;
        }
    }

    /// File-manager selection: click selects one, Ctrl toggles, Shift extends.
    fn click(&mut self, cards: &Cards, i: usize, modifiers: egui::Modifiers) {
        let id = cards.cards[i].id;
        if modifiers.shift
            && let Some(a) = self.anchor.and_then(|a| cards.index_of(a))
        {
            if !modifiers.command {
                self.selection.clear();
            }
            self.selection.extend(cards.cards[a.min(i)..=a.max(i)].iter().map(|c| c.id));
            return;
        }
        if modifiers.command {
            if !self.selection.remove(&id) {
                self.selection.insert(id);
            }
        } else {
            self.selection = HashSet::from([id]);
        }
        self.anchor = Some(id);
    }
}

struct CardStyle<'a> {
    kind: char,
    thumb: Option<&'a egui::TextureHandle>,
    /// times it is shown, with repeats in place of deleted cards
    shown: u32,
    selected: bool,
    pending: bool,
    playhead: bool,
    hovered: bool,
    pointer: Option<egui::Pos2>,
}

fn paint_card(painter: &Painter, visuals: &egui::Visuals, l: &Layout, card: &crate::edit::Card, r: Rect, s: &CardStyle) {
    let font = FontId::proportional(11.0);
    if s.pending {
        pending_band(painter, l.band_rect(r));
    }
    if !card.removed_before.is_empty() {
        // where rendered deletions were: a red line with a notch on top
        let x = r.left() - l.gap / 2.0;
        let color = visuals.error_fg_color;
        painter.vline(x, r.top()..=r.bottom(), Stroke::new(2.0, color));
        let top = pos2(x, r.top());
        painter.add(Shape::convex_polygon(vec![top, top + vec2(-4.0, -6.0), top + vec2(4.0, -6.0)], color, Stroke::NONE));
    }

    if card.deleted {
        let outline = r.shrink(0.5);
        let corners = [outline.left_top(), outline.right_top(), outline.right_bottom(), outline.left_bottom(), outline.left_top()];
        painter.extend(Shape::dashed_line(&corners, Stroke::new(1.0, visuals.weak_text_color()), 4.0, 3.0));
        if l.detailed() {
            let b = l.restore_rect(r);
            let hot = s.pointer.is_some_and(|p| b.contains(p));
            let fill = if hot { visuals.widgets.hovered.bg_fill } else { visuals.widgets.inactive.bg_fill };
            painter.circle_filled(b.center(), b.width() / 2.0, fill);
            painter.text(b.center(), Align2::CENTER_CENTER, "↺", FontId::proportional(b.width() * 0.7), visuals.strong_text_color());
        }
    } else {
        // stacked sheets for copies, on cards wide enough
        for k in (1..if r.width() >= 8.0 { s.shown.min(3) } else { 1 }).rev() {
            let sheet = r.translate(vec2(3.0, -3.0) * k as f32);
            painter.rect(
                sheet,
                1.0,
                visuals.widgets.noninteractive.bg_fill,
                Stroke::new(1.0, visuals.window_stroke.color),
                StrokeKind::Inside,
            );
        }
        match s.thumb {
            Some(t) if l.detailed() => {
                painter.image(t.id(), r, crop_uv(t.size_vec2(), r.size()), Color32::WHITE);
                // the frame type stays visible as a strip at the bottom
                let strip = Rect::from_min_max(pos2(r.left(), r.bottom() - 3.0), r.right_bottom());
                painter.rect_filled(strip, 0.0, kind_color(s.kind));
            }
            _ => {
                painter.rect_filled(r, 1.0, kind_color(s.kind));
            }
        }
        if l.detailed() {
            let galley = painter.layout_no_wrap(kind_badge(s.kind), font.clone(), Color32::WHITE);
            let badge = Rect::from_min_size(r.left_top(), galley.size() + vec2(6.0, 2.0));
            painter.rect_filled(badge, 2.0, kind_color(s.kind));
            painter.galley(badge.min + vec2(3.0, 1.0), galley, Color32::WHITE);
            if s.shown > 1 || s.hovered {
                let b = l.badge_rect(r);
                painter.rect_filled(b, 2.0, Color32::from_black_alpha(170));
                painter.text(b.center(), Align2::CENTER_CENTER, tr!("copies-badge", count = s.shown), font.clone(), Color32::WHITE);
            }
        }
    }
    if s.selected {
        painter.rect_stroke(r.expand(1.0), 2.0, Stroke::new(2.0, visuals.selection.stroke.color), StrokeKind::Outside);
    }
    if s.playhead {
        let tip = pos2(r.center().x, r.top() - 1.0);
        painter.add(Shape::convex_polygon(
            vec![tip, tip + vec2(-5.0, -6.0), tip + vec2(5.0, -6.0)],
            visuals.strong_text_color(),
            Stroke::NONE,
        ));
    }
    if l.detailed() {
        let color = if card.deleted { visuals.weak_text_color() } else { visuals.text_color() };
        let shown = painter.text(pos2(r.center().x, r.bottom() + 2.0), Align2::CENTER_TOP, tr!("frame-number", n = card.src), font, color);
        if card.deleted {
            painter.hline(shown.x_range(), shown.center().y, Stroke::new(1.0, color));
        }
    }
}

/// Texture coordinates that fill `card` with the middle of an image of size `image`.
fn crop_uv(image: Vec2, card: Vec2) -> Rect {
    let (ia, ca) = (image.x / image.y, card.x / card.y);
    let (w, h) = if ca < ia { (ca / ia, 1.0) } else { (1.0, ia / ca) };
    Rect::from_center_size(pos2(0.5, 0.5), vec2(w, h))
}

fn tooltip(cards: &Cards, i: usize, frames: &[FrameInfo], fps: f64) -> String {
    let card = &cards.cards[i];
    let mut lines = vec![tr!("card-tooltip", frame = card.src, kind = kind_name(frames[card.src].kind))];
    let shown = cards.shown(i);
    if shown > 0 {
        // where it is in the output, as edited (the next render puts it there)
        let first: usize = (0..i).map(|j| cards.shown(j) as usize).sum();
        let time = timestamp(first as f64 / fps);
        lines.push(match shown {
            1 => tr!("card-tooltip-output", frame = first, time = time),
            n => tr!("card-tooltip-output-range", from = first, to = first + n as usize - 1, time = time),
        });
    }
    if shown > 1 {
        lines.push(tr!("card-tooltip-copies", count = shown));
    }
    if shown > card.copies {
        lines.push(tr!("card-tooltip-filling", count = shown - card.copies));
    }
    if card.deleted {
        lines.push(if card.fill { tr!("card-tooltip-deleted-fill") } else { tr!("card-tooltip-deleted") });
    }
    if cards.is_pending(i) {
        lines.push(tr!("card-tooltip-pending"));
    }
    if !card.removed_before.is_empty() {
        lines.push(tr!("card-tooltip-removed", frames = frame_list(&card.removed_before)));
    }
    lines.join("\n")
}

/// The divider after a card: whole pixels, at least one once cards are
/// [`DIVIDER_PIXELS`] wide, none on narrower cards.
fn gap(zoom: f32, pixels_per_point: f32) -> f32 {
    if zoom * pixels_per_point < DIVIDER_PIXELS {
        return 0.0;
    }
    ((zoom * 0.08).min(8.0) * pixels_per_point).round().max(1.0) / pixels_per_point
}

/// "1:02.48" (minutes, seconds, hundredths), "1:02:03.48" from an hour on.
pub fn timestamp(seconds: f64) -> String {
    let hundredths = (seconds.max(0.0) * 100.0).round() as u64;
    let (secs, cs) = (hundredths / 100, hundredths % 100);
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}.{cs:02}") } else { format!("{m}:{s:02}.{cs:02}") }
}

fn kind_color(kind: char) -> Color32 {
    match kind {
        'I' => I_COLOR,
        'B' => B_COLOR,
        _ => P_COLOR,
    }
}

fn kind_badge(kind: char) -> String {
    match kind {
        'I' => tr!("frame-badge-i"),
        'P' => tr!("frame-badge-p"),
        'B' => tr!("frame-badge-b"),
        _ => tr!("frame-badge-unknown"),
    }
}

pub fn kind_name(kind: char) -> String {
    match kind {
        'I' => tr!("frame-type-i"),
        'P' => tr!("frame-type-p"),
        'B' => tr!("frame-type-b"),
        _ => tr!("frame-type-unknown"),
    }
}

/// "3, 7–9", numbers and ranges through the translation layer
fn frame_list(frames: &[usize]) -> String {
    let mut sorted = frames.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut parts = Vec::new();
    let mut i = 0;
    while i < sorted.len() {
        let mut j = i;
        while j + 1 < sorted.len() && sorted[j + 1] == sorted[j] + 1 {
            j += 1;
        }
        parts.push(if j > i { tr!("frame-range", from = sorted[i], to = sorted[j]) } else { tr!("frame-number", n = sorted[i]) });
        i = j + 1;
    }
    parts.join(&tr!("list-separator"))
}

/// yellow and dark diagonal stripes
fn pending_band(painter: &Painter, band: Rect) {
    painter.rect_filled(band, 0.0, PENDING);
    let clipped = painter.with_clip_rect(band.intersect(painter.clip_rect()));
    let stripe = Stroke::new(2.0, Color32::from_black_alpha(140));
    let mut x = band.left() - band.height();
    while x < band.right() {
        clipped.line_segment([pos2(x, band.bottom()), pos2(x + band.height(), band.top())], stripe);
        x += 6.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_lists_are_compact() {
        assert_eq!(frame_list(&[9, 3, 7, 8, 3]), "3, 7–9");
        assert_eq!(frame_list(&[1200, 1201]), "1200–1201");
    }

    #[test]
    fn dividers_are_whole_pixels_from_three_on() {
        assert_eq!(gap(2.9, 1.0), 0.0);
        assert_eq!(gap(3.0, 1.0), 1.0);
        assert_eq!(gap(2.0, 1.5), 1.0 / 1.5);
        assert_eq!(gap(100.0, 1.0), 8.0);
        assert_eq!(gap(100.0, 1.25), 8.0);
        // cards and dividers fall on the pixel grid
        let rect = Rect::from_min_size(pos2(10.0, 0.0), vec2(400.0, 150.0));
        let l = Layout::new(rect, 3.4, 0.0, 0, 1.25);
        for i in 0..50 {
            let r = l.card_rect(i);
            for x in [r.left(), r.right()] {
                assert!(((x * 1.25).round() - x * 1.25).abs() < 1e-3, "{x}");
            }
            assert!(l.card_rect(i + 1).left() - r.right() >= 0.8 - 1e-3, "a divider after card {i}");
        }
    }

    #[test]
    fn timestamps() {
        assert_eq!(timestamp(0.0), "0:00.00");
        assert_eq!(timestamp(62.48), "1:02.48");
        assert_eq!(timestamp(3723.5), "1:02:03.50");
        assert_eq!(timestamp(31.0 / 25.0), "0:01.24");
    }

    #[test]
    fn thumbnails_are_cropped_to_the_card() {
        let full = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
        assert_eq!(crop_uv(vec2(128.0, 72.0), vec2(128.0, 72.0)), full);
        let narrow = crop_uv(vec2(128.0, 72.0), vec2(36.0, 72.0));
        assert!((narrow.width() - 0.28125).abs() < 1e-6 && narrow.height() == 1.0);
    }
}
