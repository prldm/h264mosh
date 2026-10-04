//! The editable frame list behind the timeline.
//!
//! Each card shows one source frame, possibly several times. Edits are pending until
//! the next render: a deleted card stays as a tombstone, moved and re-counted cards
//! are marked. A render compiles the list into a [`Timeline`]; when it is done,
//! [`Cards::commit`] closes the gaps of the deletions it included.

use h264mosh::mosh::{Timeline, TimelineEntry, entry_seed};
use std::collections::{HashMap, HashSet};

pub type CardId = u32;

pub const MAX_COPIES: u32 = 999;

#[derive(Clone, Debug, PartialEq)]
pub struct Card {
    pub id: CardId,
    /// source frame (decode order)
    pub src: usize,
    /// times the frame is shown, at least 1
    pub copies: u32,
    /// deleted, shown as a tombstone until it is rendered
    pub deleted: bool,
    /// moved since the last render
    pub moved: bool,
    /// source frames whose deletion was rendered right before this card
    pub removed_before: Vec<usize>,
    /// revision of the last edit of this card
    rev: u64,
}

impl Card {
    /// times the frame appears in the output
    pub fn shown(&self) -> u32 {
        if self.deleted { 0 } else { self.copies }
    }
}

/// What a render was made from, to commit it when it is done.
#[derive(Clone, Debug)]
pub struct RenderSnapshot {
    rev: u64,
    /// shown copies of each card
    shown: HashMap<CardId, u32>,
}

#[derive(Clone, Debug, Default)]
pub struct Cards {
    pub cards: Vec<Card>,
    /// source frames whose deletion was rendered after the last card
    pub removed_at_end: Vec<usize>,
    /// shown copies of each card in the last render
    rendered: HashMap<CardId, u32>,
    rev: u64,
}

impl Cards {
    /// Every source frame once; this is also what counts as rendered.
    pub fn new(frames: usize) -> Self {
        let cards: Vec<Card> = (0..frames)
            .map(|src| Card { id: src as CardId, src, copies: 1, deleted: false, moved: false, removed_before: Vec::new(), rev: 0 })
            .collect();
        let rendered = cards.iter().map(|c| (c.id, 1)).collect();
        Self { cards, removed_at_end: Vec::new(), rendered, rev: 0 }
    }

    pub fn len(&self) -> usize {
        self.cards.len()
    }

    pub fn index_of(&self, id: CardId) -> Option<usize> {
        self.cards.iter().position(|c| c.id == id)
    }

    /// Changed since the last render (an edit undone by hand does not count).
    pub fn is_pending(&self, card: &Card) -> bool {
        card.moved || card.shown() != self.rendered.get(&card.id).copied().unwrap_or(0)
    }

    pub fn pending_count(&self) -> usize {
        self.cards.iter().filter(|c| self.is_pending(c)).count()
    }

    fn touch(&mut self, i: usize) -> &mut Card {
        self.rev += 1;
        let card = &mut self.cards[i];
        card.rev = self.rev;
        card
    }

    fn edit(&mut self, ids: &HashSet<CardId>, mut f: impl FnMut(&mut Card)) {
        for i in 0..self.cards.len() {
            if ids.contains(&self.cards[i].id) {
                f(self.touch(i));
            }
        }
    }

    pub fn set_deleted(&mut self, ids: &HashSet<CardId>, deleted: bool) {
        self.edit(ids, |c| c.deleted = deleted);
    }

    /// Add `delta` copies to each card (clamped to 1..=MAX_COPIES).
    pub fn add_copies(&mut self, ids: &HashSet<CardId>, delta: i64) {
        self.edit(ids, |c| c.copies = (c.copies as i64 + delta).clamp(1, MAX_COPIES as i64) as u32);
    }

    /// Move the cards `ids` (keeping their order) to just before the card at index
    /// `before` (`len()` for the end). Deletion markers stay where they were. Returns
    /// whether the order changed.
    pub fn move_to(&mut self, ids: &HashSet<CardId>, before: usize) -> bool {
        let old: Vec<CardId> = self.cards.iter().map(|c| c.id).collect();
        let before = before.min(self.cards.len());
        let at = before - self.cards[..before].iter().filter(|c| ids.contains(&c.id)).count();
        // a marker belongs to its place: hand it to the next card that stays
        let mut markers = Vec::new();
        for c in &mut self.cards {
            if ids.contains(&c.id) {
                markers.append(&mut c.removed_before);
            } else if !markers.is_empty() {
                markers.append(&mut c.removed_before);
                c.removed_before = std::mem::take(&mut markers);
            }
        }
        self.removed_at_end.splice(0..0, markers);
        let (moving, mut rest): (Vec<Card>, Vec<Card>) = std::mem::take(&mut self.cards).into_iter().partition(|c| ids.contains(&c.id));
        rest.splice(at..at, moving);
        self.cards = rest;
        let changed = self.cards.iter().map(|c| c.id).ne(old);
        if changed {
            for i in 0..self.cards.len() {
                if ids.contains(&self.cards[i].id) {
                    self.touch(i).moved = true;
                }
            }
        }
        changed
    }

    /// The output frames: (card, source frame) for every shown copy.
    pub fn output(&self) -> Vec<(CardId, usize)> {
        self.cards.iter().flat_map(|c| std::iter::repeat_n((c.id, c.src), c.shown() as usize)).collect()
    }

    /// The timeline to render, and what to commit once it is rendered. Copies of a
    /// source frame get seeds by occurrence, so they stay stable under other edits.
    pub fn timeline(&self, seed: u64) -> (Timeline, RenderSnapshot) {
        let mut occurrences: HashMap<usize, usize> = HashMap::new();
        let entries = self
            .output()
            .into_iter()
            .map(|(_, src)| {
                let n = occurrences.entry(src).or_default();
                *n += 1;
                TimelineEntry { src, effects: Vec::new(), seed: entry_seed(seed, src, *n - 1) }
            })
            .collect();
        let shown = self.cards.iter().map(|c| (c.id, c.shown())).collect();
        (Timeline { entries }, RenderSnapshot { rev: self.rev, shown })
    }

    /// A render of `snap` is done: what it showed is now the rendered state, and cards
    /// deleted before it are removed (leaving a marker on the next card). Edits made
    /// while it ran stay pending.
    pub fn commit(&mut self, snap: RenderSnapshot) {
        let mut removed = Vec::new();
        let mut kept = Vec::with_capacity(self.cards.len());
        for mut card in std::mem::take(&mut self.cards) {
            let rendered = card.rev <= snap.rev;
            if rendered && card.deleted {
                removed.append(&mut card.removed_before);
                removed.push(card.src);
                continue;
            }
            if rendered {
                card.moved = false;
            }
            if !removed.is_empty() {
                removed.append(&mut card.removed_before);
                card.removed_before = std::mem::take(&mut removed);
            }
            kept.push(card);
        }
        self.removed_at_end.append(&mut removed);
        self.cards = kept;
        self.rendered = snap.shown;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(v: &[CardId]) -> HashSet<CardId> {
        v.iter().copied().collect()
    }

    fn srcs(c: &Cards) -> Vec<usize> {
        c.output().into_iter().map(|(_, s)| s).collect()
    }

    fn render(c: &mut Cards) -> Timeline {
        let (tl, snap) = c.timeline(0);
        c.commit(snap);
        tl
    }

    #[test]
    fn delete_leaves_a_tombstone_until_rendered() {
        let mut c = Cards::new(5);
        c.set_deleted(&ids(&[1, 2]), true);
        assert_eq!(c.len(), 5);
        assert_eq!(srcs(&c), [0, 3, 4]);
        assert_eq!(c.pending_count(), 2);
        let tl = render(&mut c);
        assert_eq!(tl.entries.iter().map(|e| e.src).collect::<Vec<_>>(), [0, 3, 4]);
        assert_eq!(c.len(), 3);
        assert_eq!(c.cards[1].removed_before, [1, 2]);
        assert_eq!(c.pending_count(), 0);
    }

    #[test]
    fn restoring_or_undoing_by_hand_is_not_pending() {
        let mut c = Cards::new(3);
        c.set_deleted(&ids(&[1]), true);
        c.set_deleted(&ids(&[1]), false);
        c.add_copies(&ids(&[2]), 3);
        c.add_copies(&ids(&[2]), -3);
        assert_eq!(c.pending_count(), 0);
    }

    #[test]
    fn copies_are_clamped_and_seeded_by_occurrence() {
        let mut c = Cards::new(3);
        c.add_copies(&ids(&[1]), -5);
        assert_eq!(c.cards[1].copies, 1);
        c.add_copies(&ids(&[1]), 2);
        assert_eq!(srcs(&c), [0, 1, 1, 1, 2]);
        c.add_copies(&ids(&[0]), 5000);
        assert_eq!(c.cards[0].copies, MAX_COPIES);
        let (tl, _) = c.timeline(7);
        let seeds: Vec<u64> = tl.entries.iter().filter(|e| e.src == 1).map(|e| e.seed).collect();
        assert_eq!(seeds, (0..3).map(|k| entry_seed(7, 1, k)).collect::<Vec<_>>());
    }

    #[test]
    fn move_keeps_order_and_marks_only_real_moves() {
        let mut c = Cards::new(6);
        // [0 1 2 3 4 5] -> move 1 and 3 before 5
        assert!(c.move_to(&ids(&[3, 1]), 5));
        assert_eq!(srcs(&c), [0, 2, 4, 1, 3, 5]);
        assert_eq!(c.pending_count(), 2);
        assert!(c.cards[3].moved && c.cards[4].moved);
        // to the front, and to the end
        assert!(c.move_to(&ids(&[5]), 0));
        assert!(c.move_to(&ids(&[0]), c.len()));
        assert_eq!(srcs(&c), [5, 2, 4, 1, 3, 0]);
        render(&mut c);
        // dropping a block onto itself changes nothing
        assert!(!c.move_to(&ids(&[4, 1]), 2));
        assert!(!c.move_to(&ids(&[4, 1]), 4));
        assert_eq!(c.pending_count(), 0);
    }

    #[test]
    fn markers_stay_in_place_when_cards_move() {
        let mut c = Cards::new(5);
        c.set_deleted(&ids(&[1]), true);
        render(&mut c);
        // [0 | 2 3 4], the marker sits on card 2
        c.move_to(&ids(&[2]), c.len());
        assert_eq!(srcs(&c), [0, 3, 4, 2]);
        assert_eq!(c.cards[1].removed_before, [1]);
        assert!(c.cards[3].removed_before.is_empty());
        // deleting the card that carries markers passes them on
        c.set_deleted(&ids(&[3]), true);
        render(&mut c);
        assert_eq!(c.cards[1].removed_before, [1, 3]);
        c.set_deleted(&ids(&[4, 2]), true);
        render(&mut c);
        assert_eq!(c.removed_at_end, [1, 3, 4, 2]);
    }

    #[test]
    fn edits_made_during_a_render_stay_pending() {
        let mut c = Cards::new(4);
        c.set_deleted(&ids(&[1]), true);
        let (_, snap) = c.timeline(0);
        // while rendering: delete 2, restore 1, move 3
        c.set_deleted(&ids(&[2]), true);
        c.set_deleted(&ids(&[1]), false);
        c.move_to(&ids(&[3]), 0);
        c.commit(snap);
        assert_eq!(c.len(), 4, "1 was restored, 2 deleted after the snapshot");
        let pending: Vec<usize> = c.cards.iter().filter(|k| c.is_pending(k)).map(|k| k.src).collect();
        assert_eq!(pending, [3, 1, 2]);
    }
}
