//! The editable frame list behind the timeline.
//!
//! Each card shows one source frame, possibly several times. Edits are pending until
//! the next render: a deleted card stays as a tombstone, moved, re-counted and
//! re-effected cards are marked. A render compiles the list into a [`Timeline`]; when
//! it is done, [`Cards::commit`] closes the gaps of the deletions it included.
//!
//! A card can be deleted keeping the length: the next card shown is repeated in its
//! place, as often as the deleted one was shown, so everything after it stays at its
//! time. The tombstone remembers it, so restoring the card takes the repeats back;
//! once the deletion is rendered, the repeats become copies of that card.
//!
//! Effects are instances with parameters (one lane each). A card carries the ids of
//! the effects it gets, so a moved card takes its effects along.

use h264mosh::effects::{Fx, Mask, MbEffect};
use h264mosh::mosh::{Timeline, TimelineEntry, entry_seed};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

pub type CardId = u32;
pub type EffectId = u32;

pub const MAX_COPIES: u32 = 999;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Card {
    pub id: CardId,
    /// source frame (decode order)
    pub src: usize,
    /// times the frame is shown, at least 1
    pub copies: u32,
    /// deleted, shown as a tombstone until it is rendered
    pub deleted: bool,
    /// deleted keeping the length: the next card is shown in its place
    #[serde(default, skip_serializing_if = "is_false")]
    pub fill: bool,
    /// moved since the last render
    pub moved: bool,
    /// source frames whose deletion was rendered right before this card
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed_before: Vec<usize>,
    /// effects this card gets (applied in lane order)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<EffectId>,
    /// revision of the last edit of this card
    #[serde(skip)]
    rev: u64,
}

impl PartialEq for Card {
    /// the content; not when it was edited
    fn eq(&self, o: &Self) -> bool {
        (self.id, self.src, self.copies, self.deleted, self.fill, self.moved) == (o.id, o.src, o.copies, o.deleted, o.fill, o.moved)
            && self.removed_before == o.removed_before
            && self.effects == o.effects
    }
}

fn is_false(b: &bool) -> bool {
    !b
}

/// An effect lane: one effect with its parameters, and the area it is limited to.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Effect {
    pub id: EffectId,
    pub effect: MbEffect,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mask: Option<Mask>,
}

/// Does `effect` change a frame of this type? Motion effects, kill-intra and reference
/// shifts work on P- and B-frames, weighted prediction on P-frames only; residual,
/// quantizer, intra, transplant and deblocking effects on any frame.
pub fn affects(effect: &MbEffect, frame_kind: char) -> bool {
    match effect {
        MbEffect::ZeroResidual
        | MbEffect::Coeffs(..)
        | MbEffect::DcShift(..)
        | MbEffect::QpAdd(_)
        | MbEffect::QpSet(_)
        | MbEffect::IntraMode(_)
        | MbEffect::IntraRotate(_)
        | MbEffect::IntraRandom(_)
        | MbEffect::IntraCascade(_)
        | MbEffect::Transplant(..)
        | MbEffect::Deblock(..) => true,
        MbEffect::Weight(..) | MbEffect::Tint(..) => frame_kind == 'P',
        _ => frame_kind != 'I',
    }
}

/// Everything an edit can change: what undo restores and a project file stores.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EditState {
    pub cards: Vec<Card>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed_at_end: Vec<usize>,
    /// effect lanes, in the order they are applied
    #[serde(default)]
    pub effects: Vec<Effect>,
    /// base seed of randomized effects
    pub seed: u64,
}

/// What a card contributes to the output.
#[derive(Clone, Debug, Default, PartialEq)]
struct Look {
    shown: u32,
    effects: Vec<Fx>,
    /// the seed, when an effect is randomized
    seed: Option<u64>,
}

/// What a render was made from, to commit it when it is done.
#[derive(Clone, Debug)]
pub struct RenderSnapshot {
    rev: u64,
    looks: HashMap<CardId, Look>,
    /// the card shown in place of each deleted card that keeps the length
    fills: HashMap<CardId, CardId>,
}

#[derive(Clone, Debug, Default)]
pub struct Cards {
    pub cards: Vec<Card>,
    /// source frames whose deletion was rendered after the last card
    pub removed_at_end: Vec<usize>,
    pub effects: Vec<Effect>,
    pub seed: u64,
    /// the look of each card in the last render
    rendered: HashMap<CardId, Look>,
    rev: u64,
}

impl Cards {
    /// Every source frame once, in display order (`order`); this is also what counts
    /// as rendered.
    pub fn new(order: impl IntoIterator<Item = usize>) -> Self {
        let cards = order
            .into_iter()
            .map(|src| Card {
                id: src as CardId,
                src,
                copies: 1,
                deleted: false,
                fill: false,
                moved: false,
                removed_before: Vec::new(),
                effects: Vec::new(),
                rev: 0,
            })
            .collect();
        let mut c = Self { cards, seed: 1, ..Default::default() };
        c.rendered = (0..c.len()).map(|i| (c.cards[i].id, c.look(i))).collect();
        c
    }

    /// An edit state from a file; nothing counts as rendered yet.
    pub fn from_state(state: EditState) -> Self {
        let mut c = Self::default();
        c.restore(state);
        c
    }

    pub fn state(&self) -> EditState {
        EditState { cards: self.cards.clone(), removed_at_end: self.removed_at_end.clone(), effects: self.effects.clone(), seed: self.seed }
    }

    /// Whether the edit state is `s` (e.g. the saved one).
    pub fn is_state(&self, s: &EditState) -> bool {
        self.cards == s.cards && self.removed_at_end == s.removed_at_end && self.effects == s.effects && self.seed == s.seed
    }

    /// Go back to `s` (undo). Every card counts as edited now, so a render that is
    /// still running does not commit over it.
    pub fn restore(&mut self, s: EditState) {
        self.rev += 1;
        let rev = self.rev;
        self.cards = s.cards;
        self.cards.iter_mut().for_each(|c| c.rev = rev);
        self.removed_at_end = s.removed_at_end;
        self.effects = s.effects;
        self.seed = s.seed;
    }

    pub fn len(&self) -> usize {
        self.cards.len()
    }

    pub fn index_of(&self, id: CardId) -> Option<usize> {
        self.cards.iter().position(|c| c.id == id)
    }

    pub fn effect(&self, id: EffectId) -> Option<&Effect> {
        self.effects.iter().find(|e| e.id == id)
    }

    /// The effects `card` gets, in the order they are applied.
    pub fn card_effects(&self, card: &Card) -> Vec<Fx> {
        self.effects
            .iter()
            .filter(|e| e.enabled && card.effects.contains(&e.id))
            .map(|e| Fx { effect: e.effect.clone(), mask: e.mask.clone() })
            .collect()
    }

    /// Times card `i` appears in the output: its copies, and those of the deleted cards
    /// right before it that keep the length (and after it, if it is the last one shown).
    pub fn shown(&self, i: usize) -> u32 {
        let card = &self.cards[i];
        if card.deleted {
            return 0;
        }
        let filled = |cards: &[Card]| -> u32 { cards.iter().filter(|c| c.fill).map(|c| c.copies).sum() };
        let before = self.cards[..i].iter().rev().take_while(|c| c.deleted).count();
        let mut shown = card.copies + filled(&self.cards[i - before..i]);
        if self.cards[i + 1..].iter().all(|c| c.deleted) {
            shown += filled(&self.cards[i + 1..]);
        }
        shown
    }

    /// The card shown in place of each deleted card that keeps the length (as in
    /// [`Cards::shown`]).
    fn fill_targets(&self) -> HashMap<CardId, CardId> {
        let mut targets = HashMap::new();
        let (mut waiting, mut last) = (Vec::new(), None);
        for c in &self.cards {
            if !c.deleted {
                targets.extend(waiting.drain(..).map(|t| (t, c.id)));
                last = Some(c.id);
            } else if c.fill {
                waiting.push(c.id);
            }
        }
        if let Some(last) = last {
            targets.extend(waiting.into_iter().map(|t| (t, last)));
        }
        targets
    }

    fn look(&self, i: usize) -> Look {
        let shown = self.shown(i);
        if shown == 0 {
            return Look::default();
        }
        let card = &self.cards[i];
        let effects = self.card_effects(card);
        let seed = effects.iter().any(|e| e.effect.is_random()).then_some(self.seed);
        Look { shown, effects, seed }
    }

    /// Card `i` changed since the last render (an edit undone by hand does not count).
    pub fn is_pending(&self, i: usize) -> bool {
        let card = &self.cards[i];
        card.moved || self.rendered.get(&card.id).is_none_or(|r| *r != self.look(i))
    }

    pub fn pending_count(&self) -> usize {
        (0..self.len()).filter(|&i| self.is_pending(i)).count()
    }

    fn touch(&mut self, i: usize) -> &mut Card {
        self.rev += 1;
        let card = &mut self.cards[i];
        card.rev = self.rev;
        card
    }

    fn edit(&mut self, mut pick: impl FnMut(&Card) -> bool, mut f: impl FnMut(&mut Card)) {
        for i in 0..self.cards.len() {
            if pick(&self.cards[i]) {
                f(self.touch(i));
            }
        }
    }

    /// Delete the cards `ids`; with `keep_length`, the next card is shown in their place.
    pub fn delete(&mut self, ids: &HashSet<CardId>, keep_length: bool) {
        self.edit(
            |c| ids.contains(&c.id),
            |c| {
                c.deleted = true;
                c.fill = keep_length;
            },
        );
    }

    /// Bring back the deleted cards `ids`.
    pub fn undelete(&mut self, ids: &HashSet<CardId>) {
        self.edit(
            |c| ids.contains(&c.id),
            |c| {
                c.deleted = false;
                c.fill = false;
            },
        );
    }

    /// Add `delta` copies to each card (clamped to 1..=MAX_COPIES).
    pub fn add_copies(&mut self, ids: &HashSet<CardId>, delta: i64) {
        self.edit(|c| ids.contains(&c.id), |c| c.copies = (c.copies as i64 + delta).clamp(1, MAX_COPIES as i64) as u32);
    }

    /// Move the cards `ids` (keeping their order) to just before the card at index
    /// `before` (`len()` for the end). Deletion markers stay where they were. Returns
    /// whether the order changed.
    pub fn move_to(&mut self, ids: &HashSet<CardId>, before: usize) -> bool {
        let old: Vec<CardId> = self.cards.iter().map(|c| c.id).collect();
        let before = before.min(self.cards.len());
        let at = before - self.cards[..before].iter().filter(|c| ids.contains(&c.id)).count();
        let (moving, mut rest): (Vec<Card>, Vec<Card>) = std::mem::take(&mut self.cards).into_iter().partition(|c| ids.contains(&c.id));
        rest.splice(at..at, moving);
        if rest.iter().map(|c| c.id).eq(old.iter().copied()) {
            self.cards = rest;
            return false;
        }
        // a marker belongs to its place: a moved card hands its markers to the card that
        // followed it
        let pos: HashMap<CardId, usize> = rest.iter().enumerate().map(|(i, c)| (c.id, i)).collect();
        let mut markers = Vec::new();
        for id in &old {
            let c = &mut rest[pos[id]];
            if ids.contains(id) {
                markers.append(&mut c.removed_before);
            } else if !markers.is_empty() {
                markers.append(&mut c.removed_before);
                c.removed_before = std::mem::take(&mut markers);
            }
        }
        self.removed_at_end.splice(0..0, markers);
        self.cards = rest;
        self.edit(|c| ids.contains(&c.id), |c| c.moved = true);
        true
    }

    /// Add a new effect lane for the cards `ids`.
    pub fn add_effect(&mut self, effect: MbEffect, ids: &HashSet<CardId>) -> EffectId {
        let id = self.effects.iter().map(|e| e.id + 1).max().unwrap_or(1);
        self.effects.push(Effect { id, effect, enabled: true, mask: None });
        self.edit(|c| ids.contains(&c.id), |c| c.effects.push(id));
        id
    }

    /// Change the parameters of an effect.
    pub fn set_effect(&mut self, id: EffectId, effect: MbEffect) {
        if let Some(e) = self.effects.iter_mut().find(|e| e.id == id) {
            e.effect = effect;
            self.edit(|c| c.effects.contains(&id), |_| {});
        }
    }

    /// Limit an effect to an area of the picture, or not.
    pub fn set_effect_mask(&mut self, id: EffectId, mask: Option<Mask>) {
        if let Some(e) = self.effects.iter_mut().find(|e| e.id == id) {
            e.mask = mask;
            self.edit(|c| c.effects.contains(&id), |_| {});
        }
    }

    pub fn set_effect_enabled(&mut self, id: EffectId, enabled: bool) {
        if let Some(e) = self.effects.iter_mut().find(|e| e.id == id) {
            e.enabled = enabled;
            self.edit(|c| c.effects.contains(&id), |_| {});
        }
    }

    /// Give the effect to the cards `ids` (`on`), or take it away.
    pub fn apply_effect(&mut self, id: EffectId, ids: &HashSet<CardId>, on: bool) {
        self.edit(
            |c| ids.contains(&c.id) && c.effects.contains(&id) != on,
            |c| {
                if on {
                    c.effects.push(id);
                } else {
                    c.effects.retain(|&e| e != id);
                }
            },
        );
    }

    /// Move an effect lane up (-1) or down (+1); lanes apply from the top.
    pub fn move_effect(&mut self, id: EffectId, delta: isize) {
        let Some(i) = self.effects.iter().position(|e| e.id == id) else { return };
        let j = (i as isize + delta).clamp(0, self.effects.len() as isize - 1) as usize;
        if i != j {
            let e = self.effects.remove(i);
            self.effects.insert(j, e);
            self.edit(|c| c.effects.len() > 1, |_| {});
        }
    }

    pub fn delete_effect(&mut self, id: EffectId) {
        self.effects.retain(|e| e.id != id);
        self.edit(|c| c.effects.contains(&id), |c| c.effects.retain(|&e| e != id));
    }

    pub fn set_seed(&mut self, seed: u64) {
        self.seed = seed;
        let random: Vec<EffectId> = self.effects.iter().filter(|e| e.effect.is_random()).map(|e| e.id).collect();
        self.edit(|c| c.effects.iter().any(|e| random.contains(e)), |_| {});
    }

    /// The output frames: (card, source frame) for every shown copy.
    pub fn output(&self) -> Vec<(CardId, usize)> {
        let cards = &self.cards;
        (0..cards.len()).flat_map(|i| std::iter::repeat_n((cards[i].id, cards[i].src), self.shown(i) as usize)).collect()
    }

    /// The timeline to render, and what to commit once it is rendered. Copies of a
    /// source frame get seeds by occurrence, so they stay stable under other edits.
    pub fn timeline(&self) -> (Timeline, RenderSnapshot) {
        let mut occurrences: HashMap<usize, usize> = HashMap::new();
        let mut entries = Vec::new();
        let mut looks = HashMap::new();
        for (i, card) in self.cards.iter().enumerate() {
            let look = self.look(i);
            for _ in 0..look.shown {
                let n = occurrences.entry(card.src).or_default();
                *n += 1;
                entries.push(TimelineEntry { src: card.src, effects: look.effects.clone(), seed: entry_seed(self.seed, card.src, *n - 1) });
            }
            looks.insert(card.id, look);
        }
        (Timeline { entries }, RenderSnapshot { rev: self.rev, looks, fills: self.fill_targets() })
    }

    /// A render of `snap` is done: what it showed is now the rendered state, and cards
    /// deleted before it are removed (leaving a marker on the next card; the card shown
    /// in place of one that keeps the length takes its copies). Edits made while it ran
    /// stay pending.
    pub fn commit(&mut self, snap: RenderSnapshot) {
        let mut removed = Vec::new();
        let mut filled: HashMap<CardId, u32> = HashMap::new();
        let mut kept = Vec::with_capacity(self.cards.len());
        for mut card in std::mem::take(&mut self.cards) {
            let rendered = card.rev <= snap.rev;
            if rendered && card.deleted {
                if let Some(&target) = snap.fills.get(&card.id) {
                    *filled.entry(target).or_default() += card.copies;
                }
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
        for card in &mut kept {
            if let Some(n) = filled.get(&card.id) {
                card.copies = (card.copies + n).min(MAX_COPIES);
            }
        }
        self.removed_at_end.append(&mut removed);
        self.cards = kept;
        self.rendered = snap.looks;
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
        let (tl, snap) = c.timeline();
        c.commit(snap);
        tl
    }

    fn pending(c: &Cards) -> Vec<usize> {
        (0..c.len()).filter(|&i| c.is_pending(i)).map(|i| c.cards[i].src).collect()
    }

    #[test]
    fn delete_leaves_a_tombstone_until_rendered() {
        let mut c = Cards::new(0..5);
        c.delete(&ids(&[1, 2]), false);
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
    fn deleting_can_keep_the_length() {
        let mut c = Cards::new(0..6);
        c.add_copies(&ids(&[2]), 1);
        // 1 and 2 (twice) are filled by 3; 4 is gone; 5, the last, fills itself
        c.delete(&ids(&[1, 2]), true);
        c.delete(&ids(&[4]), false);
        assert_eq!(srcs(&c), [0, 3, 3, 3, 3, 5]);
        assert_eq!(pending(&c), [1, 2, 3, 4]);
        // restored, 2 is the next card shown after 1
        c.undelete(&ids(&[2]));
        assert_eq!(srcs(&c), [0, 2, 2, 2, 3, 5]);
        c.delete(&ids(&[2]), true);
        c.delete(&ids(&[5]), true);
        assert_eq!(srcs(&c), [0, 3, 3, 3, 3, 3]);
        // rendered, the repeats are copies of 3
        let tl = render(&mut c);
        assert_eq!(tl.entries.len(), 6);
        assert_eq!(c.cards.iter().map(|k| (k.src, k.copies)).collect::<Vec<_>>(), [(0, 1), (3, 5)]);
        assert_eq!((c.pending_count(), srcs(&c).len()), (0, 6));
        // at the end the card before is repeated; with nothing shown, nothing is
        c.delete(&ids(&[3]), true);
        assert_eq!(srcs(&c), [0; 6]);
        c.delete(&ids(&[0]), true);
        assert!(srcs(&c).is_empty());
    }

    #[test]
    fn a_fill_deleted_during_a_render_still_keeps_the_length() {
        let mut c = Cards::new(0..4);
        c.delete(&ids(&[1]), true);
        let (_, snap) = c.timeline();
        // while rendering, 2 (shown in place of 1) is deleted keeping the length too
        c.delete(&ids(&[2]), true);
        c.commit(snap);
        assert_eq!(c.cards.iter().map(|k| (k.src, k.copies, k.deleted)).collect::<Vec<_>>(), [(0, 1, false), (2, 2, true), (3, 1, false)]);
        assert_eq!(srcs(&c), [0, 3, 3, 3]);
    }

    #[test]
    fn restoring_or_undoing_by_hand_is_not_pending() {
        let mut c = Cards::new(0..3);
        c.delete(&ids(&[1]), false);
        c.undelete(&ids(&[1]));
        c.add_copies(&ids(&[2]), 3);
        c.add_copies(&ids(&[2]), -3);
        let fx = c.add_effect(MbEffect::KillIntra, &ids(&[0]));
        c.delete_effect(fx);
        assert_eq!(c.pending_count(), 0);
    }

    #[test]
    fn copies_are_clamped_and_seeded_by_occurrence() {
        let mut c = Cards::new(0..3);
        c.add_copies(&ids(&[1]), -5);
        assert_eq!(c.cards[1].copies, 1);
        c.add_copies(&ids(&[1]), 2);
        assert_eq!(srcs(&c), [0, 1, 1, 1, 2]);
        c.add_copies(&ids(&[0]), 5000);
        assert_eq!(c.cards[0].copies, MAX_COPIES);
        c.seed = 7;
        let (tl, _) = c.timeline();
        let seeds: Vec<u64> = tl.entries.iter().filter(|e| e.src == 1).map(|e| e.seed).collect();
        assert_eq!(seeds, (0..3).map(|k| entry_seed(7, 1, k)).collect::<Vec<_>>());
    }

    #[test]
    fn move_keeps_order_and_marks_only_real_moves() {
        let mut c = Cards::new(0..6);
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
        let mut c = Cards::new(0..5);
        c.delete(&ids(&[1]), false);
        render(&mut c);
        // [0 | 2 3 4], the marker sits on card 2; a move onto itself keeps it there
        assert!(!c.move_to(&ids(&[2]), 1));
        assert_eq!(c.cards[1].removed_before, [1]);
        c.move_to(&ids(&[2]), c.len());
        assert_eq!(srcs(&c), [0, 3, 4, 2]);
        assert_eq!(c.cards[1].removed_before, [1]);
        assert!(c.cards[3].removed_before.is_empty());
        // deleting the card that carries markers passes them on
        c.delete(&ids(&[3]), false);
        render(&mut c);
        assert_eq!(c.cards[1].removed_before, [1, 3]);
        c.delete(&ids(&[4, 2]), false);
        render(&mut c);
        assert_eq!(c.removed_at_end, [1, 3, 4, 2]);
    }

    #[test]
    fn edits_made_during_a_render_stay_pending() {
        let mut c = Cards::new(0..4);
        c.delete(&ids(&[1]), false);
        let (_, snap) = c.timeline();
        // while rendering: delete 2, restore 1, move 3
        c.delete(&ids(&[2]), false);
        c.undelete(&ids(&[1]));
        c.move_to(&ids(&[3]), 0);
        c.commit(snap);
        assert_eq!(c.len(), 4, "1 was restored, 2 deleted after the snapshot");
        assert_eq!(pending(&c), [3, 1, 2]);
    }

    #[test]
    fn effects_follow_their_cards_and_apply_in_lane_order() {
        let mut c = Cards::new(0..6);
        let scale = c.add_effect(MbEffect::MvScale(2.0, 2.0), &ids(&[1, 2, 3]));
        let kill = c.add_effect(MbEffect::KillIntra, &ids(&[3, 4]));
        assert_eq!(pending(&c), [1, 2, 3, 4]);
        render(&mut c);
        // moving card 2 to the end takes its effect along
        c.move_to(&ids(&[2]), c.len());
        let (tl, _) = c.timeline();
        let fx: Vec<(usize, Vec<MbEffect>)> =
            tl.entries.iter().map(|e| (e.src, e.effects.iter().map(|x| x.effect.clone()).collect())).collect();
        assert_eq!(fx[5], (2, vec![MbEffect::MvScale(2.0, 2.0)]));
        assert_eq!(fx[2], (3, vec![MbEffect::MvScale(2.0, 2.0), MbEffect::KillIntra]), "lane order");
        render(&mut c);

        // parameter changes and bypass make the cards with that effect pending
        c.set_effect(scale, MbEffect::MvScale(3.0, 1.0));
        assert_eq!(pending(&c), [1, 3, 2]);
        render(&mut c);
        c.set_effect_enabled(kill, false);
        assert_eq!(pending(&c), [3, 4]);
        c.set_effect_enabled(kill, true);
        assert!(pending(&c).is_empty());

        c.apply_effect(kill, &ids(&[0, 3]), true);
        assert_eq!(pending(&c), [0]);
        c.apply_effect(kill, &ids(&[0, 3]), false);
        assert_eq!(pending(&c), [3]);
        c.delete_effect(scale);
        assert!(c.cards.iter().all(|k| !k.effects.contains(&scale)));
        assert_eq!(c.effects.len(), 1);
    }

    #[test]
    fn only_randomized_effects_depend_on_the_seed() {
        let mut c = Cards::new(0..6);
        c.add_effect(MbEffect::MvJitter(8), &ids(&[1]));
        c.add_effect(MbEffect::ZeroResidual, &ids(&[2]));
        c.add_effect(MbEffect::IntraRandom(0.5), &ids(&[3]));
        c.add_effect(MbEffect::Transplant(0, 0.5), &ids(&[4]));
        c.add_effect(MbEffect::Transplant(0, 1.0), &ids(&[5]));
        render(&mut c);
        c.set_seed(99);
        assert_eq!(pending(&c), [1, 3, 4]);
    }

    #[test]
    fn masks_go_with_their_effect() {
        let mut c = Cards::new(0..4);
        let fx = c.add_effect(MbEffect::QpAdd(6), &ids(&[1, 2]));
        render(&mut c);
        let mask = Mask::rect(0.0, 0.5, 1.0, 0.5);
        c.set_effect_mask(fx, Some(mask.clone()));
        assert_eq!(pending(&c), [1, 2]);
        let (tl, _) = c.timeline();
        assert_eq!(tl.entries[1].effects, [Fx { effect: MbEffect::QpAdd(6), mask: Some(mask) }]);
        assert!(tl.entries[0].effects.is_empty());
        // a project file keeps it
        let json = serde_json::to_string(&c.state()).unwrap();
        assert_eq!(serde_json::from_str::<EditState>(&json).unwrap(), c.state());
        c.set_effect_mask(fx, None);
        assert!(pending(&c).is_empty());
    }

    #[test]
    fn state_roundtrips_through_json_and_restore() {
        let mut c = Cards::new(0..5);
        c.add_effect(MbEffect::MvAdd(4, -2), &ids(&[1, 2]));
        c.delete(&ids(&[3]), false);
        c.delete(&ids(&[1]), true);
        c.add_copies(&ids(&[4]), 2);
        let state = c.state();
        let json = serde_json::to_string(&state).unwrap();
        let back: EditState = serde_json::from_str(&json).unwrap();
        assert_eq!(back, state);
        let mut d = Cards::new(0..5);
        assert!(!d.is_state(&state));
        d.restore(back);
        assert!(d.is_state(&state));
        assert_eq!(d.timeline().0, c.timeline().0);
    }

    #[test]
    fn effects_skip_frame_types_they_cannot_change() {
        assert!(!affects(&MbEffect::MvScale(2.0, 2.0), 'I'));
        assert!(affects(&MbEffect::MvScale(2.0, 2.0), 'P'));
        assert!(affects(&MbEffect::MvScale(2.0, 2.0), 'B'));
        assert!(affects(&MbEffect::KillIntra, 'B'));
        assert!(!affects(&MbEffect::Weight(1.1, 0), 'B'));
        assert!(affects(&MbEffect::Transplant(3, 1.0), 'B'));
        assert!(!affects(&MbEffect::KillIntra, 'I'));
        assert!(affects(&MbEffect::ZeroResidual, 'I'));
        assert!(affects(&MbEffect::MvdScale(2.0, 2.0), 'P'));
        assert!(affects(&MbEffect::IntraMode(0), 'I'));
        assert!(affects(&MbEffect::QpAdd(6), 'I'));
        assert!(!affects(&MbEffect::Weight(1.1, 0), 'I'));
        assert!(affects(&MbEffect::Transplant(3, 1.0), 'I'));
        assert!(!affects(&MbEffect::RefShift(1), 'I'));
    }
}
