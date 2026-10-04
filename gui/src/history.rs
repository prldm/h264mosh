//! Undo and redo: snapshots of the edit state. The edits of one continuous gesture
//! (dragging a slider, turning the wheel) make one step.

use crate::edit::{CardId, EditState, EffectId};

/// Edits of the same gesture within this many seconds merge into one step.
const MERGE_SECONDS: f64 = 1.0;
/// Memory for undo steps; the oldest are dropped beyond it.
const MAX_BYTES: usize = 256 << 20;

/// A continuous gesture whose edits merge.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Gesture {
    Copies(CardId),
    SelectionCopies,
    EffectParams(EffectId),
    Seed,
}

#[derive(Default)]
pub struct History {
    undo: Vec<EditState>,
    redo: Vec<EditState>,
    /// gesture of the last edit, and when it happened
    last: Option<(Gesture, f64)>,
}

fn size(s: &EditState) -> usize {
    let card = std::mem::size_of::<crate::edit::Card>();
    s.cards.iter().map(|c| card + (c.removed_before.len() + c.effects.len()) * 8).sum::<usize>() + s.effects.len() * 64
}

impl History {
    /// Remember `before`, the state before an edit made at time `now`.
    pub fn record(&mut self, before: EditState, gesture: Option<Gesture>, now: f64) {
        let merge = gesture.is_some() && self.last.is_some_and(|(g, t)| Some(g) == gesture && now - t < MERGE_SECONDS);
        self.last = gesture.map(|g| (g, now));
        if merge {
            return;
        }
        self.undo.push(before);
        self.redo.clear();
        let mut total: usize = self.undo.iter().map(size).sum();
        while total > MAX_BYTES && self.undo.len() > 1 {
            total -= size(&self.undo.remove(0));
        }
    }

    /// The state to go back to; `current` becomes redoable.
    pub fn undo(&mut self, current: EditState) -> Option<EditState> {
        let s = self.undo.pop()?;
        self.redo.push(current);
        self.last = None;
        Some(s)
    }

    pub fn redo(&mut self, current: EditState) -> Option<EditState> {
        let s = self.redo.pop()?;
        self.undo.push(current);
        self.last = None;
        Some(s)
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::Cards;
    use std::collections::HashSet;

    fn state(copies: i64) -> EditState {
        let mut c = Cards::new(3);
        c.add_copies(&HashSet::from([0]), copies);
        c.state()
    }

    #[test]
    fn undo_redo_and_gestures() {
        let mut h = History::default();
        assert!(!h.can_undo());
        h.record(state(0), None, 0.0);
        // three wheel steps on one card, close together: one step
        h.record(state(1), Some(Gesture::Copies(0)), 1.0);
        h.record(state(2), Some(Gesture::Copies(0)), 1.5);
        h.record(state(3), Some(Gesture::Copies(0)), 2.0);
        // the same gesture after a pause is a new step
        h.record(state(4), Some(Gesture::Copies(0)), 5.0);
        assert_eq!(h.undo(state(5)), Some(state(4)));
        assert_eq!(h.undo(state(4)), Some(state(1)));
        assert_eq!(h.undo(state(1)), Some(state(0)));
        assert_eq!(h.undo(state(0)), None);
        assert_eq!(h.redo(state(0)), Some(state(1)));
        // a new edit drops what could be redone
        h.record(state(1), None, 6.0);
        assert!(!h.can_redo());
    }
}
