//! The frame strip: one card per timeline card, zoomable from thin I/P ticks up to
//! thumbnails. Click, Shift-click and Ctrl-click select, dragging moves the selection,
//! the wheel over a card's copy badge changes its count, Ctrl+wheel zooms.

use crate::edit::{CardId, Cards};
use crate::project::FrameInfo;
use crate::tr;
use eframe::egui::{self, Align2, Color32, CursorIcon, FontId, Painter, Rect, Sense, Shape, Stroke, StrokeKind, Vec2, pos2, vec2};
use std::collections::HashSet;

const MIN_ZOOM: f32 = 1.5;
/// narrower cards are plain I/P ticks without thumbnails or labels
const DETAIL_ZOOM: f32 = 24.0;
/// height of the "not rendered" band above a card
const BAND: f32 = 5.0;
const MARGIN: f32 = 4.0;
const LABEL: f32 = 14.0;
const SCROLLBAR: f32 = 8.0;
/// wheel distance for one copy more or less
const WHEEL_STEP: f32 = 40.0;

const I_COLOR: Color32 = Color32::from_rgb(236, 142, 58);
const P_COLOR: Color32 = Color32::from_rgb(92, 104, 128);
const B_COLOR: Color32 = Color32::from_rgb(150, 100, 190);
const PENDING: Color32 = Color32::from_rgb(242, 200, 40);

pub enum Event {
    /// show this card in the preview
    Seek(CardId),
    /// the cards were edited
    Edited,
}

pub struct TimelineView {
    /// card width in points (clamped to the widest cards that fit)
    pub zoom: f32,
    /// horizontal scroll in points
    scroll: f32,
    pub selection: HashSet<CardId>,
    anchor: Option<CardId>,
    /// cards being dragged
    drag: Option<HashSet<CardId>>,
    scrollbar_drag: bool,
    wheel: f32,
    /// geometry of the last frame
    last: Option<Layout>,
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
            scrollbar_drag: false,
            wheel: 0.0,
            last: None,
            max_zoom: 200.0,
        }
    }
}

/// Positions of the cards for the strip size and zoom.
#[derive(Clone, Copy)]
struct Layout {
    rect: Rect,
    zoom: f32,
    gap: f32,
    scroll: f32,
    card_h: f32,
}

impl Layout {
    fn new(rect: Rect, zoom: f32, scroll: f32) -> Self {
        let labels = if zoom >= DETAIL_ZOOM { LABEL } else { 0.0 };
        // cards fill the strip height; narrower ones show the middle of their thumbnail
        let card_h = (rect.height() - 2.0 * MARGIN - BAND - 2.0 - labels - SCROLLBAR).max(4.0);
        Self { rect, zoom, gap: gap(zoom), scroll, card_h }
    }

    /// widest cards whose thumbnails fit the strip height
    fn max_zoom(rect: Rect, aspect: f32) -> f32 {
        ((rect.height() - 2.0 * MARGIN - BAND - 2.0 - LABEL - SCROLLBAR) / aspect).max(MIN_ZOOM)
    }

    fn slot(&self) -> f32 {
        self.zoom + self.gap
    }

    fn detailed(&self) -> bool {
        self.zoom >= DETAIL_ZOOM
    }

    fn card_rect(&self, i: usize) -> Rect {
        let x = self.rect.left() + i as f32 * self.slot() - self.scroll + self.gap / 2.0;
        Rect::from_min_size(pos2(x, self.rect.top() + MARGIN + BAND + 2.0), vec2(self.zoom, self.card_h))
    }

    fn band_rect(&self, card: Rect) -> Rect {
        Rect::from_min_size(pos2(card.left(), self.rect.top() + MARGIN), vec2(card.width(), BAND))
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

impl TimelineView {
    pub fn select_all(&mut self, cards: &Cards) {
        self.selection = cards.cards.iter().map(|c| c.id).collect();
    }

    pub fn zoom_range(&self) -> std::ops::RangeInclusive<f32> {
        MIN_ZOOM..=self.max_zoom
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

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        cards: &mut Cards,
        frames: &[FrameInfo],
        thumbs: &[Option<egui::TextureHandle>],
        aspect: f32,
        playhead: Option<CardId>,
    ) -> Option<Event> {
        let (rect, response) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
        let max_zoom = Layout::max_zoom(rect, aspect);
        self.max_zoom = max_zoom;
        self.zoom = self.zoom.clamp(MIN_ZOOM, max_zoom);
        let len = cards.len();
        let mut event = None;

        // the wheel zooms around the pointer, scrolls, or changes copies over a badge
        if let Some(p) = response.hover_pos() {
            let (zoom, delta) = ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta));
            if zoom != 1.0 {
                let old_slot = self.zoom + gap(self.zoom);
                self.zoom = (self.zoom * zoom).clamp(MIN_ZOOM, max_zoom);
                let anchor = p.x - rect.left();
                self.scroll = (anchor + self.scroll) * (self.zoom + gap(self.zoom)) / old_slot - anchor;
            } else if delta != Vec2::ZERO {
                let l = Layout::new(rect, self.zoom, self.scroll);
                let i = l.index_at(p.x);
                if i < len && !cards.cards[i].deleted && l.detailed() && l.badge_rect(l.card_rect(i)).contains(p) {
                    self.wheel += delta.y + delta.x;
                    let steps = (self.wheel / WHEEL_STEP).trunc();
                    if steps != 0.0 {
                        self.wheel -= steps * WHEEL_STEP;
                        cards.add_copies(&HashSet::from([cards.cards[i].id]), steps as i64);
                        event = Some(Event::Edited);
                    }
                } else {
                    self.scroll -= delta.x + delta.y;
                }
            }
            ui.input_mut(|i| i.smooth_scroll_delta = Vec2::ZERO);
        }

        let content = len as f32 * (self.zoom + gap(self.zoom));
        let max_scroll = (content - rect.width()).max(0.0);
        self.scroll = self.scroll.clamp(0.0, max_scroll);
        let l = Layout::new(rect, self.zoom, self.scroll);
        self.last = Some(l);
        let pointer = response.interact_pointer_pos().or(response.hover_pos());

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

        // clicks: restore buttons, selection
        if response.clicked()
            && let Some(p) = pointer
            && !bar.contains(p)
        {
            let i = l.index_at(p.x);
            if i < len {
                let card = &cards.cards[i];
                let id = card.id;
                if card.deleted && l.detailed() && l.restore_rect(l.card_rect(i)).contains(p) {
                    cards.set_deleted(&HashSet::from([id]), false);
                    event = Some(Event::Edited);
                } else {
                    self.click(cards, i, ui.input(|i| i.modifiers));
                    event = Some(Event::Seek(id));
                }
            } else {
                self.selection.clear();
            }
        }

        // dragging cards
        if response.drag_started()
            && !self.scrollbar_drag
            && let Some(p) = pointer
        {
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
                if let Some(at) = insert_at
                    && cards.move_to(ids, at)
                {
                    event = Some(Event::Edited);
                }
                self.drag = None;
                insert_at = None;
            }
        }

        let hovered = pointer.filter(|p| rect.contains(*p) && !bar.contains(*p)).map(|p| l.index_at(p.x)).filter(|&i| i < len);
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, ui.visuals().extreme_bg_color);
        let visible = l.index_at(rect.left())..(l.index_at(rect.right()) + 1).min(len);
        for i in visible {
            let card = &cards.cards[i];
            let r = l.card_rect(i);
            let style = CardStyle {
                kind: frames[card.src].kind,
                thumb: thumbs.get(card.src).and_then(Option::as_ref),
                selected: self.selection.contains(&card.id),
                pending: cards.is_pending(card),
                playhead: playhead == Some(card.id),
                hovered: hovered == Some(i),
                pointer,
            };
            paint_card(&painter, ui.visuals(), &l, card, r, &style);
        }
        if let Some(at) = insert_at {
            let x = l.card_rect(at).left() - l.gap / 2.0;
            painter.vline(x, rect.y_range().shrink(MARGIN), Stroke::new(3.0, ui.visuals().selection.stroke.color));
        }
        if let Some(t) = thumb {
            painter.rect_filled(bar, 0.0, ui.visuals().faint_bg_color);
            painter.rect_filled(t.shrink(1.0), 3.0, ui.visuals().widgets.inactive.bg_fill);
        }

        if self.drag.is_none()
            && let Some(i) = hovered
        {
            response.on_hover_text_at_pointer(tooltip(cards, i, frames));
        }
        event
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
        // stacked sheets for copies
        for k in (1..card.copies.min(3)).rev() {
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
            if card.copies > 1 || s.hovered {
                let b = l.badge_rect(r);
                painter.rect_filled(b, 2.0, Color32::from_black_alpha(170));
                painter.text(b.center(), Align2::CENTER_CENTER, tr!("copies-badge", count = card.copies), font.clone(), Color32::WHITE);
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

fn tooltip(cards: &Cards, i: usize, frames: &[FrameInfo]) -> String {
    let card = &cards.cards[i];
    let mut lines = vec![tr!("card-tooltip", frame = card.src, kind = kind_name(frames[card.src].kind))];
    if card.copies > 1 {
        lines.push(tr!("card-tooltip-copies", count = card.copies));
    }
    if card.deleted {
        lines.push(tr!("card-tooltip-deleted"));
    }
    if cards.is_pending(card) {
        lines.push(tr!("card-tooltip-pending"));
    }
    if !card.removed_before.is_empty() {
        lines.push(tr!("card-tooltip-removed", frames = frame_list(&card.removed_before)));
    }
    lines.join("\n")
}

fn gap(zoom: f32) -> f32 {
    (zoom * 0.08).clamp(0.5, 8.0)
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

fn kind_name(kind: char) -> String {
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
    fn thumbnails_are_cropped_to_the_card() {
        let full = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
        assert_eq!(crop_uv(vec2(128.0, 72.0), vec2(128.0, 72.0)), full);
        let narrow = crop_uv(vec2(128.0, 72.0), vec2(36.0, 72.0));
        assert!((narrow.width() - 0.28125).abs() < 1e-6 && narrow.height() == 1.0);
    }
}
